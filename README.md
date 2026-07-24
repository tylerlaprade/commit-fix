# commit-fix

Commit-time auto-fix for Rust that is safe when several agents or editors
share one working tree. Prior art: [lint-staged](https://github.com/lint-staged/lint-staged),
minus the stash — and minus the config. There are no options.

## What it does

commit-fix exists to raise the odds a problem is fixed before CI sees it.
On every commit:

1. Every staged `.rs` file is reformatted **into the commit in flight**,
   unconditionally — the fix is rustfmt of the file's own staged content, so
   nothing happening in the working tree can block it. `cargo fmt` also runs
   across the repo, and clean unstaged files whose only change is that
   formatting ride along into the commit.
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

- Everything staged is staged **content-addressed** (`git hash-object` +
  `update-index`) from bytes this process derived from the immutable index —
  the worktree is never re-read at stage time, so a concurrent write cannot
  change what gets committed, no matter when it lands.
- Staged-file fmt and lock fixes are pure functions of the commit's own
  content, so they apply even when another session is editing the same file;
  the contended working copy is simply left alone and the two states
  converge when its session commits.
- A ride-along fix (an unstaged file swept into the commit) qualifies only
  when nobody was editing it and its working copy is byte-identical to
  rustfmt of its blob. A clippy fix applies only when the working copy still
  equals the blob its diagnostics were computed against — the one
  best-effort fixer, since its byte offsets are meaningless against any
  other content; contended files get a named warning instead.
- Partial commits (`git commit <paths>`) get their staged files fixed in the
  commit; ride-along and lock staging are suppressed there (git's throwaway
  `next-index` would keep them only as index reversals).

The floor everywhere: a fix that cannot be applied safely becomes a named
warning and CI's problem, never someone else's half-written code in your
commit, and never a blocked commit.

## Setup

```sh
cargo install commit-fix
mkdir -p .hooks
printf '#!/bin/sh\ncommit-fix\nexit 0\n' > .hooks/pre-commit
chmod +x .hooks/pre-commit
git config core.hooksPath .hooks
```
