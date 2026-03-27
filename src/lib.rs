use std::process::Command;

/// Run the lint-staged workflow: stash, format, fix, re-stage, unstash.
/// Returns Ok(()) on success, Err with a message on failure.
pub fn run() -> Result<(), String> {
    // Stash unstaged changes
    let stashed = Command::new("git")
        .args(["stash", "push", "--keep-index", "--quiet", "-m", "lint-staged-rs"])
        .status()
        .map_err(|e| format!("git stash failed: {}", e))?
        .success();

    // Format
    let _ = Command::new("cargo")
        .args(["fmt"])
        .status();

    // Auto-fix lints (clippy includes compiler lints)
    let _ = Command::new("cargo")
        .args(["clippy", "--fix", "--allow-dirty", "--allow-staged"])
        .status();

    // Re-stage fixed .rs files that were already staged
    let staged = Command::new("git")
        .args(["diff", "--name-only", "--cached", "--diff-filter=d"])
        .output()
        .map_err(|e| format!("git diff failed: {}", e))?;

    let rs_files: Vec<&str> = std::str::from_utf8(&staged.stdout)
        .unwrap_or("")
        .lines()
        .filter(|f| f.ends_with(".rs"))
        .collect();

    if !rs_files.is_empty() {
        let mut cmd = Command::new("git");
        cmd.arg("add");
        for f in &rs_files {
            cmd.arg(f);
        }
        let _ = cmd.status();
    }

    // Restore unstaged changes
    if stashed {
        let _ = Command::new("git")
            .args(["stash", "pop", "--quiet"])
            .status();
    }

    Ok(())
}
