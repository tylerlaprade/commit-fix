# commit-fix

Commit-time auto-fix for Rust that is safe when several agents or editors
share one working tree. Prior art: [lint-staged](https://github.com/lint-staged/lint-staged),
minus the stash — and minus the config. There are no options.

## What it does

On every commit, commit-fix:

1. Runs `cargo fmt` across the repo and stages each fix **into the commit in
   flight** — but only when the change is provably pure formatting (below).
2. If the commit stages Rust code or a manifest, runs `cargo clippy` in
   diagnostic mode (no scratch build — it reuses your warm target dir) and
   applies machine-applicable lint fixes via [rustfix](https://crates.io/crates/rustfix),
   staged under the same safety rules. Commits that touch no Rust never pay
   for a build.
3. If the commit changes `Cargo.toml`, freshens `Cargo.lock` and stages it —
   including the case where the repo is checked out inside an umbrella
   workspace but CI builds it standalone (resolution runs in a momentary
   scratch export with `path = "../x"` dependencies as flat siblings).

It **never blocks a commit**. Anything that fails — cargo missing, code that
doesn't compile, a lock it can't fix — degrades to a `commit-fix WARN` line
on stderr and the commit proceeds. CI stays the enforcer of record. Skipping
is git's own: `git commit --no-verify`.

## The safety model (why no stash)

Stash-based hooks corrupt working trees that more than one process writes to
— a second agent session, an editor autosave. commit-fix never stashes and
never lets ambiguous bytes into a commit:

- A file that already had unstaged modifications when the run started is
  never staged. Fixes to it stay in the working tree.
- Everything staged is staged **content-addressed** (`git hash-object` +
  `update-index`) from bytes this process derived from the immutable index —
  the worktree is never re-read at stage time, so a concurrent write cannot
  change what gets committed, no matter when it lands.
- A fmt fix qualifies only when the working copy is byte-identical to
  rustfmt of the indexed blob; a clippy fix only when the working copy still
  equals the blob its diagnostics were computed against.
- Partial commits (`git commit <paths>`) are detected (git's throwaway
  `next-index`) and the hook stands down entirely.

When a file is contended, its fix is skipped **for that commit** with a
warning — and self-heals on the next clean commit that touches it. A skipped
fix costs a CI warning; a wrongly staged fix would commit someone else's
half-written code. commit-fix always chooses the first.

## Setup

```sh
cargo install commit-fix
mkdir -p .hooks
printf '#!/bin/sh\ncommit-fix\nexit 0\n' > .hooks/pre-commit
chmod +x .hooks/pre-commit
git config core.hooksPath .hooks
```
