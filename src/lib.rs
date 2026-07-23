use std::process::Command;

/// Run the lint-staged workflow: format and fix the whole project,
/// then re-stage any .rs files that were already staged.
pub fn run() -> Result<(), String> {
    let staged_files = get_staged_rs_files()?;

    // Format everything
    let _ = Command::new("cargo").args(["fmt"]).status();

    // Auto-fix clippy + compiler lints
    let _ = Command::new("cargo")
        .args(["clippy", "--fix", "--allow-dirty", "--allow-staged"])
        .status();

    // Re-stage only files that were already staged
    re_stage(&staged_files);

    Ok(())
}

fn get_staged_rs_files() -> Result<Vec<String>, String> {
    let output = Command::new("git")
        .args(["diff", "--name-only", "--cached", "--diff-filter=d"])
        .output()
        .map_err(|e| format!("git diff failed: {e}"))?;

    Ok(std::str::from_utf8(&output.stdout)
        .unwrap_or("")
        .lines()
        .filter(|f| f.ends_with(".rs"))
        .map(String::from)
        .collect())
}

fn re_stage(files: &[String]) {
    if files.is_empty() {
        return;
    }
    let mut cmd = Command::new("git");
    cmd.arg("add");
    for f in files {
        cmd.arg(f);
    }
    let _ = cmd.status();
}
