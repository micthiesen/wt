---
name: restack
description: >-
  Resolves the conflict `wt restack` (or the TUI's `R`) bailed on and finishes
  the restack. Use on /restack, or when `wt restack` reports a conflict.
targets:
  - '*'
argument-hint: [optional branch or notes]
user_invocable: true
---
# Restack

`wt restack` (the TUI's `R` runs the same) fetches, reparents members whose
parent PR landed, then replays each member onto its parent in that member's
own worktree (`git rebase --onto <parent> <anchor> <branch>`), force-pushes
and retargets its PR. A worktree with no stacked parent is a one-member stack
on trunk. On a conflict the engine aborts cleanly, names the failing branch
and keeps its old tip as `backup/restack-*`. Your job is that conflict; when
`R` handed it to you, the clean members are already replayed.

## Before running

- Run from any member's worktree; name a branch only from outside the stack.
- Any uncommitted or untracked (non-ignored) file in a member, or a rebase in
  progress there, blocks the replay. Commit or move such files rather than
  delete them; finish or abort the rebase.
- "busy" means another restack, create or destroy holds a lock on one of
  this stack's worktrees. Wait and re-run; do not rebase around it.

## Resolving a bail

Run `wt restack` unless a bail was just handed to you; if it finishes clean,
skip to the report. Otherwise, in the failing branch's worktree:

1. If several of its commits touch the conflicted files and it will be
   squash-merged, squash first (`git reset --soft <fork point> && git commit`)
   so you resolve once. Keep the commits when their review history matters.
2. `git rebase --onto <newParent> <oldParent> <branch>`, resolving in-tree.
   Use the backup ref for the intended content, and keep both sides' intent;
   never resolve blindly to make it pass.
3. Run the project's typecheck before continuing; don't skip it.
4. `git push --force-with-lease`.
5. Re-run `wt restack` to replay the descendants; repeat until clean. No
   record edit is needed: replay cuts at the later of the stored anchor and
   the live merge-base, so your hand rebase is safe.

Then report the members, their PR states, what was reparented, and anything
still needing attention.

## Other cases

- New parent for a worktree: `wt base set <slug> <ref>` (the TUI's `b`),
  then `wt restack`. Restack only follows the recorded base.
- Different trunk for one run: `wt restack --onto <ref>`.
- An `index.lock` error: the engine already retries; re-run `wt restack`.
- Old backups: `wt restack prune-backups --days <n>`; without `--days` it
  removes all of them. A branch's backups also go when it next replays clean.

## User Instructions

$ARGUMENTS
