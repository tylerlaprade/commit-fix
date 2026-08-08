# commit-fix

Fix Rust code as you commit without disturbing concurrent work. Prior art:
[lint-staged](https://github.com/lint-staged/lint-staged), minus the stash — and
minus the config. There are no options.

## What it does

commit-fix exists to raise the odds a problem is fixed before CI sees it.
On every commit:

1. Every staged `.rs` file is reformatted **into the commit in flight**,
   unconditionally — the fix is rustfmt of the file's own staged content, so
   nothing happening in the working tree can block it. The tree is also
   formatted repo-wide, one rustfmt per tracked file (so one unparseable
   mid-edit file never blocks the rest), and clean unstaged files whose only
   change is that formatting ride along into the commit.
2. If the commit stages Rust code or a manifest, runs plain `cargo clippy`
   in diagnostic mode (no scratch build — it reuses your warm target dir)
   and applies machine-applicable fixes crate-wide via
   [rustfix](https://crates.io/crates/rustfix). Lint policy is whatever
   your `[lints]` table, `clippy.toml`, and crate attributes say —
   commit-fix passes no lint flags of its own. Lints with no automatic fix
   in the commit's own files are summarized in one warning line. A tree
   that doesn't compile — someone's mid-edit code anywhere in the dep
   graph — skips this pass *silently*: that is the editing session's
   concern, not every committer's. Commits that touch no Rust never pay
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
  other content. A contended file in the commit itself gets a named
  warning; contended bystanders are passed over silently.
- Partial commits (`git commit <paths>`) get their staged files fixed in the
  commit; ride-along and lock staging are suppressed there (git's throwaway
  `next-index` would keep them only as index reversals).

The floor everywhere: a fix that cannot be applied safely becomes a named
warning and CI's problem, never someone else's half-written code in your
commit, and never a blocked commit.

## Choosing lint rules

commit-fix has no lint configuration — it fixes whatever plain `cargo
clippy` reports. Your `[lints]` table, `clippy.toml`, and crate attributes
are the whole story; with none of those, you get clippy's defaults (no
pedantic). Opting a crate into pedantic with escape hatches looks like:

```toml
[lints.clippy]
pedantic = { level = "warn", priority = -1 }
cast_possible_truncation = "allow"
```

## Setup

```sh
cargo install commit-fix
mkdir -p .hooks
printf '#!/bin/sh\ncommit-fix\nexit 0\n' > .hooks/pre-commit
chmod +x .hooks/pre-commit
git config core.hooksPath .hooks
```
