---
name: restack
description: >-
  Rebase a worktree or stack of worktrees after a parent (or trunk) moves,
  and resolve the merge conflicts wt's squash-safe engine can't.
  User-initiated via /restack, usually after the TUI's `R` bailed on a
  conflict.
targets:
  - '*'
argument-hint: [optional branch or notes]
user_invocable: true
---
# Restack

Keep a stack of worktrees in sync with its parents, and resolve the conflicts
the engine can't.

## Mental model

Stacks are **inferred**: each worktree records the branch it's based on (its
fork base, set by `wt new --base` / `wt base`), and worktrees whose records
chain into each other form a stack. There is no other stack state. A
standalone worktree (no record, or a record pointing at trunk) is just a
one-member chain that rebases onto trunk — same command, same engine, so
everything below applies to it too.

wt's native squash-safe engine does the mechanical part — `wt restack` from any
member's worktree: it fetches, **reconciles** the records against landed PRs (a
merged parent reparents its children onto the next survivor or trunk, keeping
each child's squash-safe anchor), then **replays** every member onto its
parent: `git rebase --onto <newParent> <anchor> <branch>` in that member's own
worktree (only the member's own commits move — a squash-merged parent's
duplicate is excluded by the anchor, no patch-id guessing), force-push,
retarget the PR base. Your job is the judgment part it can't do: **resolving
conflicts**. The engine bails *clean* on conflict (aborts the rebase, leaves a
`backup/...` branch), so resolution is a normal in-tree rebase, not a rescue.

The TUI's `R` keybind runs the same `wt restack` with no model input — on
stacks and standalone worktrees alike — and escalates here, to `/restack`,
when it hits a conflict bail. So by the time this skill runs after an `R`,
the clean members are already replayed and you're here specifically for the
conflict on the named branch.

## Preconditions

- Run from inside any member's worktree (`wt restack` resolves the whole stack
  from the current branch); pass a branch explicitly only from outside.
- No uncommitted **tracked** changes in any member's worktree — replay rebases
  each in place, so it refuses if any has staged/unstaged tracked edits.
  Untracked files do NOT block; leave them alone.
- **One restack per stack at a time**: replay takes every member's
  cross-process per-worktree lock, so a busy refusal means another operation
  (a restack, create, or destroy) is touching one of THIS stack's worktrees;
  unrelated stacks restack concurrently.

## Steps

### 1. Run it
`wt restack` (add `--onto <ref>` only to override the trunk the roots land
on). If it finishes clean, go to step 3.

### 2. Resolve a conflict bail
On conflict the engine names the failing branch and a `backup/...` branch, and
leaves the tree clean. In that branch's worktree:
1. Rebase its commits onto the corrected parent
   (`git rebase --onto <newParent> <oldParent> <branch>`), resolving conflicts
   in-tree. Use the backup branch as the reference for intended content.
2. **Typecheck before continuing** — catches a bad resolution; don't skip it.
3. `git push --force-with-lease`.
4. Re-run `wt restack` to continue the descendants. Repeat until clean. Replay
   anchors each member at the *descendant-most* of its stored anchor and the
   live merge-base with its parent, so a hand-resolve is safe two ways: whether
   you rebased *off* the stored anchor or *onto newer trunk* that still
   descends from it. Either way it cuts at the true fork point and won't
   re-apply already-landed history; no record edit is needed.

Keep resolutions minimal and faithful; never resolve blindly to make it pass.

### 3. Report
Summarize the stack: member branches, PR states, what was reparented, and any
branch still needing attention.

## Notes
- Changing what a worktree is based on is `wt base set` / the TUI's `b`
  picker; restack only realigns commits with what's recorded.
- Transient `index.lock` failures (a TUI reading concurrently) are retried
  with backoff inside the engine. If one somehow surfaces, re-run `wt restack`.
- Backups self-clean: a branch's `backup/...` refs are pruned when it next
  replays clean. For leftovers, `wt restack prune-backups [--days N]`.

## User Instructions

$ARGUMENTS
