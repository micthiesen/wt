---
name: wt
description: >-
  Use the `wt` CLI/TUI to manage git worktrees: create, list, inspect, remove,
  and drive per-worktree Claude Code sessions, plus the stacked-PR workflow
  (`wt stack`). TRIGGER when the user mentions wt, worktrees, "a wt", or the
  stack/split/restack workflow. For the full split or restack flows use the
  dedicated /split and /restack skills; this is general orientation.
targets:
  - '*'
---
# wt — worktree CLI/TUI

`wt` is a terminal UI + CLI for keeping multiple git worktrees in flight at
once. A main clone stays on `main`; active branches live in per-worktree
checkouts under a configured `worktree_root`. Each row shows live git status, PR
state, preview-deploy state, issue link, and Claude Code session activity.

Config lives at `~/.config/wt/config.toml` (`wt` refuses to start without it;
the loader reports every missing field at once). The standard install aliases
`wt='~/.wt/bin/wt'`; if `wt` isn't found in a non-interactive shell, invoke
`~/.wt/bin/wt` directly.

## Subcommands

- `wt` — interactive TUI (vim keys: `j`/`k`/Enter, `?` for help).
- `wt ls` — list worktrees.
- `wt new <input>` — create a worktree (and branch). `--base <ref>` forks from a
  non-trunk parent and records it (used as the worktree's diff/display base).
- `wt rm [slug]` — remove a worktree (optionally its branch).
- `wt clean` — bulk-remove merged/gone worktrees.
- `wt doctor [slug]` — health report (dirty, sync, PR, merged).
- `wt open [slug]` — open a worktree in the editor.
- `wt size [paths…] [--json]` — production-LOC + file count for a diff (excludes
  tests/snapshots/generated/lockfiles). On a holistic branch with a manifest it
  breaks size down per slice.
- `wt base <slug> | set <slug> <ref> | clear <slug>` — show/set/clear a
  worktree's recorded fork base.
- `wt logs [slug]` — tail background-destroy logs.

Every subcommand runs non-interactively when stdout isn't a TTY. Run
`wt <command> --help` for per-command options.

## The stacked-PR workflow (`wt stack`)

`wt` turns one validated branch into a stack (or parallel lanes) of small,
reviewable draft PRs, then keeps them rebased. The manifest in wt's state is the
single source of truth; the engine materializes and replays from it.

- `wt stack context` — read-only pre-split context for the current worktree
  (branch, base decision, changed files, `wt size`). Used by /split.
- `wt stack hunks [--holistic <b>] [--unified <n>] <file>…` — content-hashed hunk
  ids for hunk-level slice partitions.
- `wt stack plan --from <file>` / `apply [--from <file>] [--verify]` — strict-
  ingest a manifest, then materialize worktrees + draft PRs (`--verify`
  typechecks each cumulative prefix in a throwaway worktree first).
- `wt stack status [stackId] [--all]` — render the manifest DAG (a fork renders
  as a tree) + drift vs reality. Defaults to the current branch's stack.
- `wt stack section <stackId> <idOrPr> [label]` — the static PR-body "Stack"
  section for one slice.
- `wt stack reconcile` / `replay` / `rebase` — manifest bookkeeping, squash-safe
  replay, and the combined one-shot used by /restack.
- `wt stack split` / `add` — reshape a live slice into sub-slices, or append an
  existing branch as a new tip slice.

For the end-to-end flows, prefer the dedicated skills:
- **/split** — carve a holistic branch into a stack and open the draft PRs.
- **/restack** — rebase the stack on main, repair drift, resolve conflicts.

Design rationale + the manifest contract: the wt repo's
`docs/stacking-workflow.md`.

## User Instructions

$ARGUMENTS
