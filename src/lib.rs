//! commit-fix: commit-time auto-fix that is safe when several agents or
//! editors share one working tree.
//!
//! Contract:
//! - Never blocks and has no configuration: every failure degrades to a
//!   stderr warning and the commit proceeds. CI is the enforcer of record;
//!   `git commit --no-verify` is the skip.
//! - No stashing, and never stages foreign content. A file with unstaged
//!   modifications from before the run is skipped outright. Everything else
//!   is staged content-addressed (`git hash-object` + `update-index`) from
//!   bytes this process derived from the immutable index — the worktree is
//!   never re-read at stage time, so a concurrent write can change nothing
//!   about what gets committed.
//! - `cargo fmt` runs repo-wide; a fix is staged only when the working copy
//!   is byte-identical to rustfmt of the indexed blob (provably pure
//!   formatting).
//! - Clippy fixes run whenever the commit stages Rust code or a manifest:
//!   `cargo clippy --message-format=json` in the real tree (no scratch
//!   build, reuses the warm target dir; touches no source, though cargo may
//!   refresh a stale Cargo.lock as part of resolution — the lock pass
//!   tolerates that), machine-applicable suggestions applied via rustfix to
//!   the indexed blob, gated the same way.
//! - A commit that changes Cargo.toml gets Cargo.lock freshened. Non-
//!   workspace repos are resolved in a scratch export with `path = "../x"`
//!   dependencies as flat siblings — a repo checked out inside an umbrella
//!   workspace would otherwise resolve the umbrella's lock instead of its
//!   own (the one its standalone CI uses).

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
    status_ok(
        "git",
        &["update-index", "--add", "--cacheinfo", &spec],
        None,
    )
}

/// Edition from a literal `edition = "NNNN"`; workspace-inherited or dotted
/// forms fall through to a default that only affects whether a fix can be
/// staged (a mismatch fails the purity gate — safe), never what is staged.
fn manifest_edition(dir: &Path) -> String {
    std::fs::read_to_string(dir.join("Cargo.toml"))
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                let rest = l.trim().strip_prefix("edition")?.trim_start().strip_prefix('=')?;
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
        let root =
            std::env::temp_dir().join(format!("commit-fix-{}-{nanos}", std::process::id()));
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

/// Run clippy in diagnostic mode (writes nothing, reuses the warm target
/// dir) and stage its machine-applicable fixes, each applied via rustfix to
/// the indexed blob and gated on an untouched working copy.
fn clippy_fix(pre_wip: &HashSet<String>, edition: &str) {
    let Ok(out) = Command::new("cargo")
        .args(["clippy", "--message-format=json"])
        .stderr(Stdio::null())
        .output()
    else {
        warn("cargo clippy unavailable; lint fixes skipped");
        return;
    };
    let text = String::from_utf8_lossy(&out.stdout);
    let mut compile_error = !out.status.success();
    let mut by_file: HashMap<String, Vec<rustfix::Suggestion>> = HashMap::new();
    for line in text.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v["reason"] != "compiler-message" {
            continue;
        }
        let msg = &v["message"];
        if msg["level"] == "error" {
            compile_error = true;
        }
        let Ok(sugs) = rustfix::get_suggestions_from_json(
            &msg.to_string(),
            &HashSet::new(),
            rustfix::Filter::MachineApplicableOnly,
        ) else {
            continue;
        };
        for s in sugs {
            let files: HashSet<String> = s
                .solutions
                .iter()
                .flat_map(|sol| sol.replacements.iter())
                .map(|r| r.snippet.file_name.clone())
                .collect();
            // Single-file, repo-relative suggestions only: anything else
            // (cross-file, absolute, or escaping the repo) is skipped.
            if files.len() == 1 {
                let f = files.into_iter().next().unwrap();
                if !f.starts_with('/') && !f.starts_with("..") {
                    by_file.entry(f).or_default().push(s);
                }
            }
        }
    }
    if compile_error {
        warn("clippy hit compile errors; lint fixes skipped (CI will tell)");
        return;
    }
    let mut staged = Vec::new();
    for (file, sugs) in by_file {
        if pre_wip.contains(&file) {
            warn(&format!("clippy fix for {file} skipped: file has local edits"));
            continue;
        }
        // Suggestions carry byte offsets into the compiled source; they are
        // valid against the indexed blob only while the working copy (what
        // clippy compiled) still equals it.
        let Some(blob) = index_blob(&file) else {
            continue;
        };
        if std::fs::read(&file).map(|w| w != blob).unwrap_or(true) {
            warn(&format!("clippy fix for {file} skipped: file has local edits"));
            continue;
        }
        let Ok(code) = String::from_utf8(blob) else {
            continue;
        };
        let Ok(fixed) = rustfix::apply_suggestions(&code, &sugs) else {
            warn(&format!("clippy fix for {file} did not apply cleanly; skipped"));
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

/// Freshen Cargo.lock for the pending commit. Resolution is authoritative
/// and staging is decided by byte-compare against the staged lock — never by
/// `--locked` against the tree, which cargo's own resolution during the
/// clippy pass may already have satisfied. Ownership is judged against
/// `lock_at_start` (the lock bytes before this run touched anything): only
/// pre-existing local edits back us off; cargo's mid-run rewrites are
/// machine noise this pass supersedes.
fn freshen_lock(repo_root: &Path, pre_wip: &HashSet<String>, lock_at_start: Option<Vec<u8>>) {
    let is_workspace = std::fs::read_to_string(repo_root.join("Cargo.toml"))
        .is_ok_and(|s| s.lines().any(is_workspace_header));
    let staged_lock = index_blob("Cargo.lock");
    if pre_wip.contains("Cargo.lock") || staged_lock != lock_at_start {
        if !cargo_in(repo_root, &["metadata", "--locked", "--format-version", "1"]) {
            warn("Cargo.lock is stale but has local edits; leaving it alone (CI will fail)");
        }
        return;
    }

    let new_bytes = if is_workspace {
        // In-place: the root lock is the right lock for a workspace root.
        if !cargo_in(repo_root, &["metadata", "--format-version", "1"]) {
            warn("workspace Cargo.lock could not be resolved (CI will fail)");
            return;
        }
        let Ok(bytes) = std::fs::read(repo_root.join("Cargo.lock")) else {
            return;
        };
        bytes
    } else {
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
        let _ = std::fs::write(repo_root.join("Cargo.lock"), &bytes);
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
    if std::env::set_current_dir(&repo_root).is_err() || !repo_root.join("Cargo.toml").exists() {
        return;
    }

    // Partial commit (`git commit <pathspec>`): git runs hooks against a
    // throwaway next-index and folds only the pathspec paths back, so
    // anything staged here becomes a silent revert later. Stand down.
    if std::env::var("GIT_INDEX_FILE").is_ok_and(|f| {
        Path::new(&f)
            .file_name()
            .is_some_and(|n| n.to_string_lossy().starts_with("next-index-"))
    }) {
        warn("partial commit: auto-fix skipped");
        return;
    }

    // Nothing staged (message-only amend etc.)? A failure here means no HEAD
    // yet (initial commit) — proceed in that case.
    if status_ok("git", &["diff", "--cached", "--quiet"], None) {
        return;
    }

    // Unstaged-modified files before we touch anything: never stage these.
    let pre_wip: HashSet<String> = git_paths(&["diff", "--name-only", "-z"])
        .unwrap_or_default()
        .into_iter()
        .collect();
    // Lock bytes before this run: the clippy pass's cargo invocation may
    // legitimately rewrite a stale lock, and that must not read as WIP.
    let lock_at_start = std::fs::read(repo_root.join("Cargo.lock")).ok();

    let edition = manifest_edition(&repo_root);

    // Tree-wide format, then stage only provably-pure formatting changes,
    // content-addressed from the validated bytes.
    if status_ok("cargo", &["fmt"], None) {
        let mut staged = Vec::new();
        for f in git_paths(&["diff", "--name-only", "-z", "--", "*.rs"]).unwrap_or_default() {
            if pre_wip.contains(&f) {
                continue;
            }
            let Some(want) = pure_fmt_bytes(&f, &edition) else {
                continue;
            };
            if stage_bytes(&f, &want) {
                staged.push(f);
            }
        }
        if !staged.is_empty() {
            eprintln!("commit-fix: rustfmt fixed {}", staged.join(" "));
        }
    } else {
        warn("cargo fmt failed; formatting unchecked for this commit");
    }

    // Staged paths; on a no-HEAD initial commit fall back to the full index.
    let staged_names = git_paths(&["diff", "--cached", "--name-only", "-z"])
        .or_else(|| git_paths(&["ls-files", "-z"]))
        .unwrap_or_default();
    let commits_rust = staged_names
        .iter()
        .any(|p| p.ends_with(".rs") || p.ends_with("Cargo.toml") || p.ends_with("Cargo.lock"));
    let commits_manifest = staged_names.iter().any(|p| p.ends_with("Cargo.toml"));

    // Clippy needs a build — only pay for it when the commit touches Rust.
    if commits_rust {
        clippy_fix(&pre_wip, &edition);
    }
    if commits_manifest {
        freshen_lock(&repo_root, &pre_wip, lock_at_start);
    }
}
