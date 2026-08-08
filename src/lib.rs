//! commit-fix: commit-time auto-fix built to raise the odds a problem is
//! fixed before CI sees it, in trees that several agents or editors write
//! to at once.
//!
//! Two hard floors, everything else best-effort:
//! - Never blocks: every failure degrades to a stderr warning and the
//!   commit proceeds. CI is the enforcer of record; `git commit
//!   --no-verify` is the skip. No configuration exists.
//! - Never stages foreign bytes: everything staged is content-addressed
//!   (`git hash-object` + `update-index`) from bytes this process derived
//!   from the immutable index — the worktree is never re-read at stage
//!   time, so a concurrent write can change nothing about what gets
//!   committed.
//!
//! What gets fixed:
//! - Every staged `.rs` file is reformatted unconditionally: the staged
//!   fix is rustfmt of the file's own indexed blob, a pure function of the
//!   commit's content, so worktree contention is irrelevant to it. A
//!   contended worktree copy is simply left alone (states converge when
//!   its session commits). The tree is also formatted repo-wide — one
//!   rustfmt per tracked file, so a single unparseable mid-edit file never
//!   blocks the rest — and clean unstaged files whose only change is that
//!   formatting ride along, gated on being byte-identical to rustfmt of
//!   their blob.
//! - Clippy runs whenever the commit stages Rust code or a manifest:
//!   plain `cargo clippy --message-format=json` in the real tree, reusing
//!   the warm target dir (cargo may refresh a stale Cargo.lock as part of
//!   resolution — the lock pass tolerates that). Lint policy — pedantic
//!   level, whitelisted lints — belongs in each repo's `[lints.clippy]`
//!   manifest table, the one place every tool reads; this binary passes no
//!   lint flags of its own. Machine-applicable suggestions are applied crate-wide via
//!   rustfix to indexed blobs — auto-fixable means safe to apply
//!   everywhere, so fixes ride along with whatever commit is in flight
//!   (files with local edits are passed over, silently unless the file is
//!   the commit's own). Lints with no automatic fix are summarized in one
//!   warning covering only the commit's staged files. Per-repo clippy.toml
//!   and crate attributes are honored natively. If the tree doesn't
//!   compile — someone's mid-edit code, anywhere in the dep graph — the
//!   pass skips SILENTLY: that is the editing session's concern, not every
//!   committer's.
//! - A commit that changes Cargo.toml gets Cargo.lock freshened and
//!   staged. Non-workspace repos are resolved in a scratch export with
//!   `path = "../x"` dependencies as flat siblings — a repo checked out
//!   inside an umbrella workspace would otherwise resolve the umbrella's
//!   lock instead of its own (the one its standalone CI uses).
//!
//! Partial commits (`git commit <paths>`): git runs the hook against a
//! throwaway next-index holding only the pathspec files, then re-stages
//! those files' pre-hook bytes afterward. Staged-file fixes therefore land
//! in the commit; ride-along and lock staging are suppressed (they would
//! survive only as index reversals). The pathspec files themselves are
//! left `MM` — real index holding the unfixed pre-hook bytes — which the
//! next hook run re-fixes, collapsing the difference.

use std::collections::{HashMap, HashSet};
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn warn(msg: &str) {
    eprintln!("commit-fix WARN: {msg}");
}

/// Capture stdout of a command; None on spawn failure or non-zero exit.
fn output(cmd: &str, args: &[&str]) -> Option<Vec<u8>> {
    let out = Command::new(cmd).args(args).output().ok()?;
    out.status.success().then_some(out.stdout)
}

fn status_ok(cmd: &str, args: &[&str], cwd: Option<&Path>) -> bool {
    let mut c = Command::new(cmd);
    c.args(args);
    if let Some(d) = cwd {
        c.current_dir(d);
    }
    c.stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok_and(|s| s.success())
}

/// NUL-separated path list from git; None if git itself failed.
fn git_paths(args: &[&str]) -> Option<Vec<String>> {
    let out = output("git", args)?;
    Some(
        String::from_utf8_lossy(&out)
            .split('\0')
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect(),
    )
}

/// The staged blob for a path, from the index.
fn index_blob(path: &str) -> Option<Vec<u8>> {
    output("git", &["show", &format!(":{path}")])
}

fn index_mode(path: &str) -> String {
    output("git", &["ls-files", "-s", "--", path])
        .and_then(|o| {
            String::from_utf8_lossy(&o)
                .split_whitespace()
                .next()
                .map(String::from)
        })
        .filter(|m| !m.is_empty())
        .unwrap_or_else(|| "100644".to_string())
}

/// Stage exact bytes for a path without re-reading the worktree: write the
/// blob to the object store, then point the index at it. A concurrent
/// worktree write cannot change what gets committed.
fn stage_bytes(path: &str, bytes: &[u8]) -> bool {
    let Ok(mut child) = Command::new("git")
        .args(["hash-object", "-w", "--stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    let fed = child
        .stdin
        .take()
        .is_some_and(|mut s| s.write_all(bytes).is_ok());
    let Ok(out) = child.wait_with_output() else {
        return false;
    };
    if !fed || !out.status.success() {
        return false;
    }
    let oid = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let spec = format!("{},{oid},{path}", index_mode(path));
    // Another process's momentary `git add` holds the index lock; retry a
    // few times before giving the fix up as a warning.
    for attempt in 0..5 {
        if status_ok(
            "git",
            &["update-index", "--add", "--cacheinfo", &spec],
            None,
        ) {
            return true;
        }
        if attempt < 4 {
            std::thread::sleep(std::time::Duration::from_millis(50 << attempt));
        }
    }
    warn(&format!("could not stage fix for {path} (index busy)"));
    false
}

/// Edition from a literal `edition = "NNNN"`; workspace-inherited or dotted
/// forms fall through to a default that only affects whether a fix can be
/// staged (a mismatch fails the purity gate — safe), never what is staged.
fn manifest_edition(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("Cargo.toml"))
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                let rest = l
                    .trim()
                    .strip_prefix("edition")?
                    .trim_start()
                    .strip_prefix('=')?;
                let val = rest.trim().trim_matches('"');
                (!val.is_empty() && val.bytes().all(|b| b.is_ascii_digit()))
                    .then(|| val.to_string())
            })
        })
        .unwrap_or_else(|| "2024".to_string())
}

/// rustfmt applied to bytes; None if rustfmt failed (e.g. edition mismatch).
fn rustfmt(bytes: &[u8], edition: &str) -> Option<Vec<u8>> {
    let mut child = Command::new("rustfmt")
        .args(["--edition", edition, "--emit", "stdout"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    child.stdin.take()?.write_all(bytes).ok()?;
    let out = child.wait_with_output().ok()?;
    out.status.success().then_some(out.stdout)
}

/// The validated pure-formatting bytes for `path`: rustfmt of the indexed
/// blob, but only when the working copy already equals them.
fn pure_fmt_bytes(path: &str, edition: &str) -> Option<Vec<u8>> {
    let blob = index_blob(path)?;
    let want = rustfmt(&blob, edition)?;
    (std::fs::read(path).ok()? == want).then_some(want)
}

/// True when the working copy of `path` is exactly rustfmt of its indexed
/// blob — i.e. the on-disk change is provably pure formatting.
#[must_use]
pub fn is_pure_fmt(path: &str, edition: &str) -> bool {
    pure_fmt_bytes(path, edition).is_some()
}

/// True for a `[workspace]` table header, tolerating inner whitespace and a
/// trailing comment.
fn is_workspace_header(line: &str) -> bool {
    let l = line.split('#').next().unwrap_or("");
    let compact: String = l.chars().filter(|c| !c.is_whitespace()).collect();
    compact == "[workspace]"
}

/// `path = "../x"` dependency names from a manifest: inline-table deps and
/// `path` keys in multi-line dependency tables.
fn sibling_deps(manifest_dir: &Path) -> Vec<String> {
    let Ok(text) = std::fs::read_to_string(manifest_dir.join("Cargo.toml")) else {
        return Vec::new();
    };
    let mut deps: Vec<String> = text
        .lines()
        .filter_map(|l| {
            let l = l.split('#').next().unwrap_or("");
            let (key, rest) = l.split_once('=')?;
            let key = key.trim();
            let is_path_key = key == "path" || key.ends_with(".path");
            let inline = rest.contains("path");
            if !is_path_key && !inline {
                return None;
            }
            let (_, after) = rest.split_once("\"../")?;
            let name = after.split('"').next()?;
            (!name.is_empty() && !name.contains('/')).then(|| name.to_string())
        })
        .collect();
    deps.sort();
    deps.dedup();
    deps
}

/// Extract a git tree-ish into `dst` (git archive | tar -x).
fn export_tree(repo: &Path, treeish: &str, dst: &Path) -> bool {
    std::fs::create_dir_all(dst).is_ok()
        && (|| {
            let mut archive = Command::new("git")
                .args(["-C", &repo.to_string_lossy(), "archive", treeish])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            let tar_in = archive.stdout.take()?;
            let tar = Command::new("tar")
                .args(["-x", "-C", &dst.to_string_lossy()])
                .stdin(tar_in)
                .status()
                .ok();
            let arch = archive.wait().ok()?;
            (arch.success() && tar.is_some_and(|t| t.success())).then_some(())
        })()
        .is_some()
}

/// Ephemeral scratch mirroring a standalone CI checkout: the pending commit
/// at <scratch>/self with the recursive ../ path-dep closure as flat
/// siblings. Used only for lockfile resolution (cargo metadata — no build),
/// so it lives for seconds and holds sources only.
struct Scratch {
    root: PathBuf,
}

impl Scratch {
    fn build(repo_root: &Path) -> Option<Scratch> {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .ok()?
            .subsec_nanos();
        let root = std::env::temp_dir().join(format!("commit-fix-{}-{nanos}", std::process::id()));
        std::fs::create_dir_all(&root).ok()?;
        let scratch = Scratch { root };

        let tree = String::from_utf8_lossy(&output("git", &["write-tree"])?)
            .trim()
            .to_string();
        if !export_tree(repo_root, &tree, &scratch.self_dir()) {
            warn("could not export the pending commit");
            return None;
        }
        let parent = repo_root.parent()?;
        let mut queue = sibling_deps(&scratch.self_dir());
        let mut seen = HashSet::new();
        while let Some(dep) = queue.pop() {
            if !seen.insert(dep.clone()) {
                continue;
            }
            let dst = scratch.root.join(&dep);
            if !export_tree(&parent.join(&dep), "HEAD", &dst) {
                warn(&format!("could not export sibling dependency {dep}"));
                return None;
            }
            queue.extend(sibling_deps(&dst));
        }
        Some(scratch)
    }

    fn self_dir(&self) -> PathBuf {
        self.root.join("self")
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn cargo_in(dir: &Path, args: &[&str]) -> bool {
    status_ok("cargo", args, Some(dir))
}

/// Run clippy in diagnostic mode (reuses the warm target dir) and stage its
/// machine-applicable fixes for the commit's own staged files, each applied
/// via rustfix to the indexed blob and gated on an untouched working copy.
/// Diagnostic paths arrive relative to the CARGO WORKSPACE root (or
/// absolute), not the git repo — for a repo that is a member of an
/// enclosing workspace those differ. Resolve to repo-relative; None for
/// files outside this repo (e.g. sibling path-dep diagnostics).
fn repo_rel(file: &str, repo_root: &Path, ws_root: &Path) -> Option<String> {
    let p = Path::new(file);
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        ws_root.join(p)
    };
    abs.strip_prefix(repo_root)
        .ok()
        .map(|r| r.to_string_lossy().into_owned())
}

fn lint_code(message: &serde_json::Value) -> Option<&str> {
    let code = message["code"]["code"].as_str()?;
    let bytes = code.as_bytes();
    let hard_error_code =
        bytes.len() == 5 && bytes[0] == b'E' && bytes[1..].iter().all(u8::is_ascii_digit);
    (!hard_error_code).then_some(code)
}

fn clippy_fix(
    repo_root: &Path,
    staged_set: &HashSet<&str>,
    pre_wip: &HashSet<String>,
    edition: &str,
    pathspec_mode: bool,
) {
    let ws_root = output(
        "cargo",
        &["locate-project", "--workspace", "--message-format", "plain"],
    )
    .and_then(|o| {
        Path::new(String::from_utf8_lossy(&o).trim())
            .parent()
            .map(Path::to_path_buf)
    })
    .and_then(|p| std::fs::canonicalize(p).ok())
    .unwrap_or_else(|| repo_root.to_path_buf());
    let Ok(out) = Command::new("cargo")
        .args(["clippy", "--message-format=json"])
        .stderr(Stdio::null())
        .output()
    else {
        return; // no cargo — the staged fmt pass already warned
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut compile_error = false;
    let mut lint_error = false;
    let mut by_file: HashMap<String, Vec<rustfix::Suggestion>> = HashMap::new();
    let mut unfixable: Vec<String> = Vec::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v["reason"] != "compiler-message" {
            continue;
        }
        let msg = &v["message"];
        if msg["level"] == "error" {
            if lint_code(msg).is_some() {
                lint_error = true;
            } else {
                compile_error = true;
            }
        }
        let sugs = rustfix::get_suggestions_from_json(
            &msg.to_string(),
            &HashSet::new(),
            rustfix::Filter::MachineApplicableOnly,
        )
        .unwrap_or_default();
        if sugs.is_empty() {
            // A real lint with no machine-applicable fix, in a file this
            // commit stages: remember it for the report. Debt in files the
            // commit doesn't touch is not this committer's noise (a repo
            // can carry hundreds of outstanding pedantic notes).
            let raw = msg["spans"][0]["file_name"].as_str().unwrap_or("?");
            let Some(at) = repo_rel(raw, repo_root, &ws_root) else {
                continue;
            };
            if (msg["level"] == "warning" || msg["level"] == "error")
                && staged_set.contains(at.as_str())
                && lint_code(msg).is_some_and(|c| c.starts_with("clippy::"))
            {
                let code = msg["code"]["code"].as_str().unwrap_or("clippy");
                let ln = msg["spans"][0]["line_start"].as_u64().unwrap_or(0);
                unfixable.push(format!("{code} at {at}:{ln}"));
            }
            continue;
        }
        for s in sugs {
            let files: HashSet<String> = s
                .solutions
                .iter()
                .flat_map(|sol| sol.replacements.iter())
                .map(|r| r.snippet.file_name.clone())
                .collect();
            // Single-file suggestions inside this repo only: cross-file
            // suggestions and sibling-dep files are skipped.
            if files.len() == 1 {
                let f = files.into_iter().next().unwrap();
                if let Some(rel) = repo_rel(&f, repo_root, &ws_root) {
                    by_file.entry(rel).or_default().push(s);
                }
            }
        }
    }
    if !out.status.success() && !lint_error {
        compile_error = true;
    }
    if compile_error {
        // Someone's mid-edit code doesn't compile. That is their session's
        // concern, not this committer's — skip lint fixes silently.
        return;
    }
    if !unfixable.is_empty() {
        unfixable.sort();
        unfixable.dedup();
        let shown = unfixable.iter().take(10).cloned().collect::<Vec<_>>();
        let more = unfixable.len().saturating_sub(10);
        let suffix = if more > 0 {
            format!(" (+{more} more)")
        } else {
            String::new()
        };
        warn(&format!("clippy: {}{suffix}", shown.join(", ")));
    }
    let mut staged = Vec::new();
    for (file, sugs) in by_file {
        // Auto-fixable means safe to apply everywhere: fixes sweep the
        // whole crate, riding along with whatever commit is in flight. In
        // pathspec mode only the commit's own files (anything else would
        // survive just as an index reversal).
        if pathspec_mode && !staged_set.contains(file.as_str()) {
            continue;
        }
        if pre_wip.contains(&file) {
            // Only the committer's own staged files earn a warning; a
            // busy tree always has someone's WIP somewhere.
            if staged_set.contains(file.as_str()) {
                warn(&format!(
                    "clippy fix for {file} skipped: file has local edits"
                ));
            }
            continue;
        }
        // Suggestions carry byte offsets into the compiled source; they are
        // valid against the indexed blob only while the working copy (what
        // clippy compiled) still equals it.
        let Some(blob) = index_blob(&file) else {
            continue;
        };
        if std::fs::read(&file).map_or(true, |w| w != blob) {
            warn(&format!(
                "clippy fix for {file} skipped: file has local edits"
            ));
            continue;
        }
        let Ok(code) = String::from_utf8(blob) else {
            continue;
        };
        let Ok(fixed) = rustfix::apply_suggestions(&code, &sugs) else {
            warn(&format!(
                "clippy fix for {file} did not apply cleanly; skipped"
            ));
            continue;
        };
        let fixed = rustfmt(fixed.as_bytes(), edition).unwrap_or_else(|| fixed.into_bytes());
        if fixed == code.as_bytes() {
            continue;
        }
        if std::fs::write(&file, &fixed).is_ok() && stage_bytes(&file, &fixed) {
            staged.push(file);
        }
    }
    if !staged.is_empty() {
        eprintln!("commit-fix: clippy fixed {}", staged.join(" "));
    }
}

/// Freshen Cargo.lock for the pending commit. The staged bytes are derived
/// from the commit's own manifest (index content), so staging is
/// unconditional — decided by byte-compare against the staged lock, never
/// by `--locked` against the tree, which cargo's own resolution during the
/// clippy pass may already have satisfied. The tree copy is only synced
/// when it was untouched since hook start (`lock_at_start`); a contended
/// tree lock is left alone.
fn freshen_lock(repo_root: &Path, lock_at_start: Option<Vec<u8>>) {
    let is_workspace = std::fs::read_to_string(repo_root.join("Cargo.toml"))
        .is_ok_and(|s| s.lines().any(is_workspace_header));
    let staged_lock = index_blob("Cargo.lock");

    let new_bytes = if is_workspace {
        // In-place: the root lock is the right lock for a workspace root.
        // Resolution rewrites the tree lock, so back off when the tree copy
        // carries someone's pre-run edits.
        if staged_lock != lock_at_start {
            if !cargo_in(
                repo_root,
                &["metadata", "--locked", "--format-version", "1"],
            ) {
                warn("workspace Cargo.lock is stale but has local edits; leaving it alone (CI will fail)");
            }
            return;
        }
        if !cargo_in(repo_root, &["metadata", "--format-version", "1"]) {
            warn("workspace Cargo.lock could not be resolved (CI will fail)");
            return;
        }
        let Ok(bytes) = std::fs::read(repo_root.join("Cargo.lock")) else {
            return;
        };
        bytes
    } else {
        // Standalone: resolve in the scratch export — index-derived, so
        // tree contention never blocks the staged fix.
        let Some(scratch) = Scratch::build(repo_root) else {
            return;
        };
        let selfd = scratch.self_dir();
        if !cargo_in(&selfd, &["metadata", "--format-version", "1"]) {
            warn("Cargo.lock could not be resolved standalone (CI will fail)");
            return;
        }
        let Ok(bytes) = std::fs::read(selfd.join("Cargo.lock")) else {
            return;
        };
        // Sync the tree copy only when it was untouched at hook start.
        if lock_at_start == staged_lock {
            let _ = std::fs::write(repo_root.join("Cargo.lock"), &bytes);
        }
        bytes
    };

    if staged_lock.as_deref() != Some(new_bytes.as_slice()) && stage_bytes("Cargo.lock", &new_bytes)
    {
        eprintln!("commit-fix: refreshed Cargo.lock");
    }
}

pub fn run() {
    let Some(root) = output("git", &["rev-parse", "--show-toplevel"]) else {
        return; // not a git repo
    };
    let repo_root = PathBuf::from(String::from_utf8_lossy(&root).trim());
    // Canonical form, so workspace-relative diagnostic paths strip cleanly
    // even when temp dirs or symlinks give the two roots different spellings.
    let repo_root = std::fs::canonicalize(&repo_root).unwrap_or(repo_root);
    if std::env::set_current_dir(&repo_root).is_err() || !repo_root.join("Cargo.toml").exists() {
        return;
    }

    // Partial commit (`git commit <pathspec>`): the throwaway next-index
    // means only fixes to the staged (pathspec) files survive into the
    // commit; anything else staged here would linger as an index reversal.
    let pathspec_mode = std::env::var("GIT_INDEX_FILE").is_ok_and(|f| {
        Path::new(&f)
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("next-index-"))
    });

    // Nothing staged (message-only amend etc.)? A failure here means no HEAD
    // yet (initial commit) — proceed in that case.
    if status_ok("git", &["diff", "--cached", "--quiet"], None) {
        return;
    }

    // Unstaged-modified files before we touch anything: ride-along cleanup
    // never stages these (their tree state is someone's work in progress).
    let pre_wip: HashSet<String> = git_paths(&["diff", "--name-only", "-z"])
        .unwrap_or_default()
        .into_iter()
        .collect();
    // Lock bytes before this run: the clippy pass's cargo invocation may
    // legitimately rewrite a stale lock, and that must not read as WIP.
    let lock_at_start = std::fs::read(repo_root.join("Cargo.lock")).ok();

    let edition = manifest_edition(&repo_root);

    // Staged paths; on a no-HEAD initial commit fall back to the full index.
    let staged_names = git_paths(&["diff", "--cached", "--name-only", "-z"])
        .or_else(|| git_paths(&["ls-files", "-z"]))
        .unwrap_or_default();
    let staged_set: HashSet<&str> = staged_names.iter().map(String::as_str).collect();

    // Tree-wide format, one rustfmt per tracked file (parallelized).
    // Deliberately not `cargo fmt`: that is all-or-nothing, so one
    // unparseable mid-edit file anywhere would kill formatting for the
    // whole tree. Per file, broken ones stay as they are (the staged pass
    // warns for anything actually being committed).
    let all_rs = git_paths(&["ls-files", "-z", "--", "*.rs"]).unwrap_or_default();
    std::thread::scope(|scope| {
        for chunk in all_rs.chunks(all_rs.len().div_ceil(8).max(1)) {
            let edition = &edition;
            scope.spawn(move || {
                for f in chunk {
                    let _ = status_ok("rustfmt", &["--edition", edition, f], None);
                }
            });
        }
    });

    // Staged .rs files are fixed unconditionally: the staged bytes are
    // rustfmt of each file's own indexed blob — a pure function of the
    // commit's content — so worktree contention is irrelevant here. This
    // needs no working `cargo fmt` either; rustfmt alone suffices.
    let mut staged = Vec::new();
    for f in staged_names.iter().filter(|p| p.ends_with(".rs")) {
        let Some(blob) = index_blob(f) else {
            continue; // e.g. staged deletion
        };
        let Some(want) = rustfmt(&blob, &edition) else {
            warn(&format!("could not format staged {f} (CI will fail)"));
            continue;
        };
        if want != blob && stage_bytes(f, &want) {
            staged.push(f.clone());
        }
    }

    // Ride-along cleanup of unstaged files: only provably-pure formatting
    // changes on files nobody was editing, and never in pathspec mode
    // (git would keep them only as index reversals).
    if !pathspec_mode {
        for f in git_paths(&["diff", "--name-only", "-z", "--", "*.rs"]).unwrap_or_default() {
            if staged_set.contains(f.as_str()) || pre_wip.contains(&f) {
                continue;
            }
            let Some(want) = pure_fmt_bytes(&f, &edition) else {
                continue;
            };
            if stage_bytes(&f, &want) {
                staged.push(f);
            }
        }
    }
    if !staged.is_empty() {
        eprintln!("commit-fix: rustfmt fixed {}", staged.join(" "));
    }

    let commits_rust = staged_names
        .iter()
        .any(|p| p.ends_with(".rs") || p.ends_with("Cargo.toml") || p.ends_with("Cargo.lock"));
    let commits_manifest = staged_names.iter().any(|p| p.ends_with("Cargo.toml"));

    // Clippy needs a build — only pay for it when the commit touches Rust.
    if commits_rust {
        clippy_fix(&repo_root, &staged_set, &pre_wip, &edition, pathspec_mode);
    }
    if commits_manifest {
        if pathspec_mode && !staged_set.contains("Cargo.lock") {
            // Staging the lock into the next-index would only linger as a
            // reversal; the commit ships whatever lock it staged (or none).
            warn("partial commit changes Cargo.toml without Cargo.lock; lock not freshened (CI may fail)");
        } else {
            freshen_lock(&repo_root, lock_at_start);
        }
    }
}
