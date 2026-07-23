//! Scenario tests: each builds a throwaway git repo and runs the real binary
//! the way a pre-commit hook would, then inspects the index.

use std::path::{Path, PathBuf};
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_commit-fix");
const UNFORMATTED: &str = "pub fn probe( x:i32 ) ->  i32 {   x+ 1 }\n";
const FORMATTED: &str = "pub fn probe(x: i32) -> i32 {\n    x + 1\n}\n";

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
    assert_eq!(staged_blob(&dir, "src/lib.rs"), FORMATTED);
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
fn pathspec_commit_stands_down() {
    let dir = make_repo("pathspec");
    write(&dir, "src/lib.rs", UNFORMATTED);
    sh(&dir, "git", &["add", "src/lib.rs"]);
    let fake_index = dir.join(".git/next-index-42.lock");
    std::fs::copy(dir.join(".git/index"), &fake_index).unwrap();
    let mut c = Command::new(BIN);
    c.current_dir(&dir)
        .env("GIT_INDEX_FILE", &fake_index);
    let out = c.output().unwrap();
    assert!(out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("partial commit"));
    assert_eq!(staged_blob(&dir, "src/lib.rs"), UNFORMATTED, "must not touch the index");
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
    assert!(String::from_utf8_lossy(&out.stderr).contains("cargo fmt failed"));
}

