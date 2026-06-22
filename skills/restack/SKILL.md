---
name: restack
description: >-
  Rebase and repair the current stack after edits or a merge. Drives
  `wt stack reconcile`/`replay`/`rebase`, and resolves merge conflicts when the
  native squash-safe engine bails to a backup branch. User-initiated via /restack.
targets:
  - '*'
argument-hint: [optional stackId or notes]
user_invocable: true
---
# Restack

Keep a stack in sync with `main` and with mid-stack edits, and resolve the
conflicts the engine can't.

## Current State

!`WT="$HOME/.wt/bin/wt"; [ -x "$WT" ] || WT=wt; "$WT" stack status`

## Mental model

After a split, the stack is the source of truth. wt's **native** squash-safe
engine does the mechanical part: per slice, `git rebase --onto <newParent>
<anchor> <branch>` in that slice's own worktree (so only the slice's own
commits move — a squash-merged parent's duplicate is excluded by the anchor,
no patch-id guessing), force-push, then retarget the PR base. Your job is the
judgment part it can't do — **resolving conflicts** and keeping the manifest
honest. The engine bails *clean* on conflict (aborts the rebase, leaves a
`backup/...` branch), so resolution is a normal in-tree rebase, not a rescue.

wt exposes the pieces so you can step through a messy restack: `wt stack
reconcile` (manifest bookkeeping only — mark merged, reparent children),
`wt stack replay` (the squash-safe replay), and `wt stack rebase` (reconcile
then replay — the normal one-shot). Reach for the granular two when a conflict
interrupts a replay.

Note: cleaning a merged slice's worktree in the TUI (`c`) already runs
`reconcileStack` automatically, so an after-merge restack usually starts
**already reconciled** — the orphaned children have been reparented onto trunk
and just need a `replay` to actually rebase their commits off the squashed
parent. Running `rebase` is still fine (reconcile is idempotent).

The TUI's `R` keybind is the **algorithmic fast path**: it runs the whole stack
through `wt stack rebase` (fetch + reconcile + squash-safe replay) with no model
input, and only escalates here — to `/restack` — when it hits a conflict bail.
So by the time this skill runs after an `R`, the clean slices are already
replayed and you're here specifically for the conflict on the named slice.

## Preconditions

- Run from inside any slice worktree. `wt stack rebase`/`status` resolve the
  `stackId` from the current branch, so you rarely pass it — give it explicitly
  (lower-kebab, e.g. `eng-1234` — not the branch name) only from outside the stack.
  `wt stack status` thus shows **only the current stack** by default; pass `--all`
  for the global view. You almost never need `--all` here — a restack operates on
  one stack, and the global dump invites commenting on unrelated stacks (every
  stack has a "slice 04", so cross-stack references collide).
- No uncommitted **tracked** changes in any **open** slice's worktree. Replay
  rebases each slice in place in its own worktree, so it refuses if any has
  staged/unstaged tracked edits — commit or stash first. Untracked files do NOT
  block; leave them alone. **Planned slices don't block and don't replay**: a
  slice with no PR yet is skipped with a notice (its branch/worktree, if any, is
  WIP the engine leaves alone — it catches up at `wt stack apply` / `wt stack
  add`). **One restack at a time**: replay takes a cross-process lock, so never
  run two concurrently.

## Steps

### 1. Inspect
Read the `wt stack status` above. Note slice order, each PR's state, and any
**drift** (live PR base or branch parent disagreeing with the manifest). Also
sanity-check the repair state before rebasing: `git status` should be clean (no
uncommitted tracked changes), no rebase is mid-flight (`git status` will say so),
and `git branch --list 'backup/*'` shows leftover backups from prior bails.

### 2. Rebase / repair
Run `wt stack rebase` (add `--onto <base>` only to override the root base).
`<base>` defaults to `main`; pass the **parent branch** if this stack is still
based on an unmerged parent PR (rebase onto main only once the parent lands). It
reconciles the manifest against landed PRs (merged slices marked, children
reparented), then replays every surviving slice onto its parent and retargets
the PR bases. A slice's copy of a now-squash-merged parent commit is **dropped
automatically** by the anchor — that duplicate is not a conflict you resolve by
hand. If it finishes clean, go to step 4.

### 3. Resolve a conflict bail
If the engine exits with a conflict, it names the failing slice branch and a
`backup/...` branch, and leaves the tree clean. For that slice:
1. Check out the slice branch in its worktree.
2. Rebase its commits onto the corrected parent
   (`git rebase --onto <newParent> <oldParent> <branch>`). Resolve conflicts
   in-tree. Use the holistic tag (`<stackId>-holistic`) or the backup branch as
   the reference for intended content; if intent is unclear, read the original
   conversation via the manifest's `holisticSessionId`.
3. **Typecheck before continuing** — catches a bad resolution; don't skip it.
   Slice worktrees are created install-free, so either install deps in the slice
   worktree first (you're actively editing it now, so deps are worth it) or
   typecheck the resolved branch from a dep-having checkout.
4. `git push --force-with-lease`. (Replay backstops this: a slice that needs
   no rebase but whose remote lags the local tip gets pushed automatically —
   but pushing here keeps the remote state obvious while you work.)
5. Re-run `wt stack replay` to continue descendants (the manifest is already
   reconciled; no need to reconcile again). Repeat until clean. Replay anchors
   each slice at the *descendant-most* of its stored anchor and the live
   merge-base with its parent, so a hand-resolve is safe two ways: whether you
   rebased the slice *off* the stored anchor, or *onto newer trunk* that still
   descends from it (e.g. `main` advanced mid-restack). Either way it cuts at the
   true fork point — it won't re-apply trunk's already-landed history — and no
   manifest edit is needed. If `main` moved while you were resolving, just run
   `wt stack rebase` again: it re-fetches and the same anchor logic keeps the
   replay clean. (If you ever must rebase a slice fully by hand, the equivalent
   is `git rebase --onto <currentParentTip> <merge-base(branch,parent)> <branch>`
   — cut at the live fork point, not the stale stored base.)

Keep resolutions minimal and faithful; never resolve blindly to make it pass.

### 4. Report
Summarize **only the stack you restacked**: slice branches, PR states, what
merged, what was reparented, and any slice still needing attention. Don't
volunteer drift or status on other stacks — that's out of scope and invites
confusion. If you ever do need to reference another stack, never use a bare
"slice N" (every stack has one); always pair it with the stack id **and** the PR
number, e.g. "eng-5238 / #4945".

## Notes
- Deciding/changing the stack *shape* is `/split`, not here. This skill operates
  on an existing manifest.
- Transient `index.lock` failures (a TUI reading concurrently) are retried with
  backoff inside the engine — both at rebase start and on the abort path — so
  they should never surface. If one somehow does, just re-run `wt stack replay`.
- Backups self-clean: a slice's `backup/...` branches are pruned when it next
  replays clean. For leftovers (e.g. from merged-and-gone slices), run
  `wt stack prune-backups [--days N]`.
- If `wt` stack commands aren't available yet, report that and stop — don't
  hand-rebase a stack without the engine and the manifest update.
- Design rationale + open questions: the wt repo's `docs/stacking-workflow.md`.

## User Instructions

$ARGUMENTS
