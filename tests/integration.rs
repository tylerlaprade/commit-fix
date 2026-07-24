//! Scenario tests: each builds a throwaway git repo and runs the real binary
//! the way a pre-commit hook would, then inspects the index.

use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_commit-fix");
const UNFORMATTED: &str = "pub fn probe( x:i32 ) ->  i32 {   x+ 1 }\n";
const FORMATTED: &str = "pub fn probe(x: i32) -> i32 {\n    x + 1\n}\n";
// What the full pipeline produces: fmt plus clippy's must_use_candidate fix.
const FIXED: &str = "#[must_use]\npub fn probe(x: i32) -> i32 {\n    x + 1\n}\n";

fn sh(dir: &Path, cmd: &str, args: &[&str]) -> std::process::Output {
    let out = Command::new(cmd)
        .args(args)
        .current_dir(dir)
        .output()
        .unwrap_or_else(|e| panic!("{cmd} {args:?}: {e}"));
    assert!(
        out.status.success(),
        "{cmd} {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn git(dir: &Path, args: &[&str]) -> String {
    String::from_utf8_lossy(&sh(dir, "git", args).stdout).into_owned()
}

fn write(dir: &Path, rel: &str, content: &str) {
    let p = dir.join(rel);
    std::fs::create_dir_all(p.parent().unwrap()).unwrap();
    std::fs::write(p, content).unwrap();
}

/// Fresh package repo with one committed lib.rs; returns its path.
fn make_repo(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "cfx-test-{name}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .subsec_nanos()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    sh(&dir, "git", &["init", "-q"]);
    for (k, v) in [
        ("user.email", "t@t"),
        ("user.name", "t"),
        ("commit.gpgsign", "false"),
    ] {
        sh(&dir, "git", &["config", k, v]);
    }
    write(
        &dir,
        "Cargo.toml",
        &format!("[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n"),
    );
    write(&dir, "src/lib.rs", "pub fn base() {}\n");
    sh(&dir, "git", &["add", "-A"]);
    sh(&dir, "git", &["commit", "-qm", "init"]);
    dir
}

fn run_hook(dir: &Path, envs: &[(&str, &str)]) {
    let mut c = Command::new(BIN);
    c.current_dir(dir).env_remove("GIT_INDEX_FILE");
    for (k, v) in envs {
        c.env(k, v);
    }
    let out = c.output().unwrap();
    assert!(out.status.success(), "hook must always exit 0");
}

fn staged_blob(dir: &Path, rel: &str) -> String {
    git(dir, &["show", &format!(":{rel}")])
}

#[test]
fn stages_pure_fmt_fix() {
    let dir = make_repo("fmtfix");
    write(&dir, "src/lib.rs", UNFORMATTED);
    sh(&dir, "git", &["add", "src/lib.rs"]);
    run_hook(&dir, &[]);
    assert_eq!(staged_blob(&dir, "src/lib.rs"), FIXED);
}

#[test]
fn never_stages_preexisting_wip() {
    let dir = make_repo("wip");
    write(&dir, "src/other.rs", "pub fn committed() {}\n");
    sh(&dir, "git", &["add", "-A"]);
    sh(&dir, "git", &["commit", "-qm", "add other"]);
    // Staged work in lib.rs; unstaged WIP in other.rs.
    write(&dir, "src/lib.rs", UNFORMATTED);
    sh(&dir, "git", &["add", "src/lib.rs"]);
    write(&dir, "src/other.rs", "pub fn committed( ) {}\n");
    run_hook(&dir, &[]);
    let staged = git(&dir, &["diff", "--cached", "--name-only"]);
    assert!(staged.contains("src/lib.rs"));
    assert!(!staged.contains("src/other.rs"), "WIP file must stay unstaged");
    assert_eq!(staged_blob(&dir, "src/other.rs"), "pub fn committed() {}\n");
}

#[test]
fn purity_gate_rejects_foreign_content() {
    let dir = make_repo("gate");
    write(&dir, "src/lib.rs", UNFORMATTED);
    sh(&dir, "git", &["add", "src/lib.rs"]);
    // The gate compares worktree bytes to rustfmt(indexed blob); test both
    // sides from a subprocess-free context by chdir (sole cwd-touching test).
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(&dir).unwrap();
    write(&dir, "src/lib.rs", FORMATTED);
    assert!(commit_fix::is_pure_fmt("src/lib.rs", "2021"));
    write(
        &dir,
        "src/lib.rs",
        &format!("{FORMATTED}pub fn foreign() {{}}\n"),
    );
    assert!(
        !commit_fix::is_pure_fmt("src/lib.rs", "2021"),
        "foreign content must never pass the gate"
    );
    std::env::set_current_dir(prev).unwrap();
}

#[test]
fn pathspec_commit_gets_fixed() {
    // A real `git commit <path>` with the binary installed as the real
    // pre-commit hook, so git's temp-index reconciliation is exercised.
    let dir = make_repo("pathspec");
    let hooks = dir.join("hookdir");
    std::fs::create_dir_all(&hooks).unwrap();
    std::fs::write(
        hooks.join("pre-commit"),
        format!("#!/bin/sh\n{BIN}\nexit 0\n"),
    )
    .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(hooks.join("pre-commit"), std::fs::Permissions::from_mode(0o755))
            .unwrap();
    }
    sh(&dir, "git", &["config", "core.hooksPath", &hooks.to_string_lossy()]);
    write(&dir, "src/lib.rs", UNFORMATTED);
    sh(&dir, "git", &["commit", "-qm", "pathspec", "src/lib.rs"]);
    assert_eq!(
        git(&dir, &["show", "HEAD:src/lib.rs"]),
        FIXED,
        "pathspec commit must ship the fixes"
    );
    // Git re-stages the pathspec file's PRE-hook bytes after the commit, so
    // the file is left MM (index = unfixed bytes, tree = cargo-fmt'd). The
    // next hook run re-formats that blob, collapsing the difference.
    assert_eq!(
        git(&dir, &["status", "--porcelain", "--", "src/lib.rs"]),
        "MM src/lib.rs\n"
    );
}

#[test]
fn contended_staged_file_still_gets_fixed() {
    // Another session's edits on top of the staged file must not block the
    // staged fix, and must never leak into it.
    let dir = make_repo("contended");
    write(&dir, "src/lib.rs", UNFORMATTED);
    sh(&dir, "git", &["add", "src/lib.rs"]);
    write(
        &dir,
        "src/lib.rs",
        &format!("{UNFORMATTED}pub fn foreign_wip() {{}}\n"),
    );
    run_hook(&dir, &[]);
    let blob = staged_blob(&dir, "src/lib.rs");
    assert_eq!(blob, FORMATTED, "staged content must be fixed despite contention");
    assert!(!blob.contains("foreign_wip"), "foreign bytes must never be staged");
    let tree = std::fs::read_to_string(dir.join("src/lib.rs")).unwrap();
    assert!(tree.contains("foreign_wip"), "the WIP tree copy must survive");
}

#[test]
fn regenerates_standalone_lock_with_sibling_dep() {
    let parent = make_repo("parent-scope"); // just to get a unique temp parent
    let parent = parent.parent().unwrap().join(format!(
        "cfx-lockland-{}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&parent);
    std::fs::create_dir_all(&parent).unwrap();

    // Sibling dependency crate, its own git repo.
    let dep = parent.join("depcrate");
    std::fs::create_dir_all(&dep).unwrap();
    sh(&dep, "git", &["init", "-q"]);
    for (k, v) in [("user.email", "t@t"), ("user.name", "t"), ("commit.gpgsign", "false")] {
        sh(&dep, "git", &["config", k, v]);
    }
    write(&dep, "Cargo.toml", "[package]\nname = \"depcrate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n");
    write(&dep, "src/lib.rs", "pub fn dep() {}\n");
    sh(&dep, "git", &["add", "-A"]);
    sh(&dep, "git", &["commit", "-qm", "init"]);

    // Main crate: lock generated BEFORE the dep is added, then Cargo.toml
    // gains the path dep and is staged — the lock is now stale.
    let main = parent.join("maincrate");
    std::fs::create_dir_all(&main).unwrap();
    sh(&main, "git", &["init", "-q"]);
    for (k, v) in [("user.email", "t@t"), ("user.name", "t"), ("commit.gpgsign", "false")] {
        sh(&main, "git", &["config", k, v]);
    }
    write(&main, "Cargo.toml", "[package]\nname = \"maincrate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\n");
    write(&main, "src/lib.rs", "pub fn m() {}\n");
    sh(&main, "cargo", &["metadata", "--format-version", "1"]);
    sh(&main, "git", &["add", "-A"]);
    sh(&main, "git", &["commit", "-qm", "init"]);
    write(&main, "Cargo.toml", "[package]\nname = \"maincrate\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n[dependencies]\ndepcrate = { path = \"../depcrate\" }\n");
    sh(&main, "git", &["add", "Cargo.toml"]);
    run_hook(&main, &[]);
    let staged = git(&main, &["diff", "--cached", "--name-only"]);
    assert!(staged.contains("Cargo.lock"), "lock must be staged: {staged}");
    assert!(staged_blob(&main, "Cargo.lock").contains("depcrate"));
    let _ = std::fs::remove_dir_all(&parent);
}

#[test]
fn clippy_fix_is_staged_when_safe() {
    let dir = make_repo("clippy");
    let lint = "pub fn has(v: &[i32], x: i32) -> bool {\n    v.iter().any(|a| *a == x)\n}\n";
    write(&dir, "src/lib.rs", lint);
    sh(&dir, "git", &["add", "src/lib.rs"]);
    run_hook(&dir, &[]);
    let blob = staged_blob(&dir, "src/lib.rs");
    assert!(blob.contains("v.contains(&x)"), "clippy fix not staged: {blob}");
    assert_eq!(std::fs::read_to_string(dir.join("src/lib.rs")).unwrap(), blob);
}

#[test]
fn fixable_lint_in_clean_unstaged_file_rides_along() {
    let dir = make_repo("sweep");
    write(&dir, "src/lib.rs", "pub mod extra;\npub fn base() {}\n");
    write(&dir, "src/extra.rs", "pub fn gives() -> i32 {\n    5\n}\n");
    sh(&dir, "git", &["add", "-A"]);
    sh(&dir, "git", &["commit", "-qm", "add extra"]);
    // Stage an unrelated change; extra.rs is committed, clean, and carries
    // a machine-fixable must_use_candidate.
    write(
        &dir,
        "src/lib.rs",
        "pub mod extra;\npub fn base() {}\npub fn more() {}\n",
    );
    sh(&dir, "git", &["add", "src/lib.rs"]);
    run_hook(&dir, &[]);
    assert!(
        staged_blob(&dir, "src/extra.rs").contains("#[must_use]"),
        "crate-wide fixable lint must ride along"
    );
}

#[test]
fn unfixable_clippy_warning_is_reported() {
    let dir = make_repo("lintreport");
    // clippy::missing_panics_doc: pedantic, fires on pub items, no auto-fix.
    let code = "pub fn head(v: &[i32]) -> i32 {\n    *v.first().unwrap()\n}\n";
    write(&dir, "src/lib.rs", code);
    sh(&dir, "git", &["add", "src/lib.rs"]);
    let out = Command::new(BIN).current_dir(&dir).output().unwrap();
    assert!(out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("missing_panics_doc"),
        "pedantic lint must appear in the report: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn broken_tree_skips_clippy_silently() {
    let dir = make_repo("brokentree");
    write(&dir, "src/lib.rs", "pub mod part;\npub fn base() {}\n");
    write(&dir, "src/part.rs", "pub fn whole() {}\n");
    sh(&dir, "git", &["add", "-A"]);
    sh(&dir, "git", &["commit", "-qm", "add module"]);
    // Stage an unformatted change; another session breaks part.rs unstaged.
    write(&dir, "src/lib.rs", &format!("pub mod part;\n{UNFORMATTED}"));
    sh(&dir, "git", &["add", "src/lib.rs"]);
    write(&dir, "src/part.rs", "pub fn half(");
    let out = Command::new(BIN).current_dir(&dir).output().unwrap();
    assert!(out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        !err.to_lowercase().contains("clippy"),
        "a tree someone else broke must not produce clippy noise: {err}"
    );
    // The staged fmt fix still applies.
    assert!(staged_blob(&dir, "src/lib.rs").contains("pub fn probe(x: i32) -> i32 {"));
}

#[test]
fn never_blocks_when_cargo_missing() {
    let dir = make_repo("nocargo");
    write(&dir, "src/lib.rs", UNFORMATTED);
    sh(&dir, "git", &["add", "src/lib.rs"]);
    let out = Command::new(BIN)
        .current_dir(&dir)
        .env("PATH", "/usr/bin:/bin")
        .output()
        .unwrap();
    assert!(out.status.success(), "must exit 0 without cargo");
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("could not format staged"),
        "the staged file's unfixed state must be warned about"
    );
    assert_eq!(staged_blob(&dir, "src/lib.rs"), UNFORMATTED);
}

