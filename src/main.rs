fn main() {
    // No flags, no env vars: `git commit --no-verify` is the skip.
    // run() never blocks a commit — all failures degrade to warnings.
    commit_fix::run();
}
