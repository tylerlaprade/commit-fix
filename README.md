# lint-staged-rs

Auto-format and lint-fix staged Rust files on commit. The Rust equivalent of [lint-staged](https://github.com/lint-staged/lint-staged).

## What it does

On every commit, lint-staged-rs:

1. Stashes your unstaged changes (so only staged code is affected)
2. Runs `cargo fmt`
3. Runs `cargo clippy --fix` (auto-fixes both compiler and clippy lints)
4. Re-stages the fixed files
5. Restores your unstaged changes

## Setup

Add to your `Cargo.toml`:

```toml
[dev-dependencies]
husky-rs = "0.3"
```

Install the binary:

```sh
cargo install lint-staged-rs
```

Create `.husky/pre-commit`:

```sh
#!/bin/sh
lint-staged-rs
```

Run `cargo build` once to let husky-rs configure git hooks. Done.

## How it works

[husky-rs](https://github.com/pplmx/husky-rs) manages git hook installation (sets `core.hooksPath` to `.husky/`). lint-staged-rs is the pre-commit script that does the formatting and fixing.

The stash/unstash workflow ensures that if you have partially staged files, only the staged version gets formatted. Your unstaged work-in-progress is preserved.

## Skipping

Set `NO_HUSKY_HOOKS=1` to skip the hook (useful in CI):

```sh
NO_HUSKY_HOOKS=1 git commit -m "skip hooks"
```
