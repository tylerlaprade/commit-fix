fn main() {
    if std::env::var("NO_HUSKY_HOOKS").as_deref() == Ok("1") {
        return;
    }

    if let Err(e) = lint_staged_rs::run() {
        eprintln!("lint-staged-rs: {}", e);
        std::process::exit(1);
    }
}
