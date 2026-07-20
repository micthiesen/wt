# Stacked PRs

A stacked PR is just a branch based on another branch. wt keeps that
relationship as one small per-worktree record and derives everything else from
it — there is no managed stack state, no registration step, and nothing to
keep in sync.

## The base record

Every worktree can carry a **fork base**: the branch it's based on, plus the
fork-point SHA (`baseBranch` / `baseSha` in wt's state file). It's written
three ways:

- `wt new <input> --base <ref>` — records the parent and the fork point at
  creation.
- `wt base set <slug> <ref>` / the TUI's `b` picker — backfill or change it by
  hand (record only; nothing is rebased).
- restacks — a reconcile rewrites the parent when it lands; a replay advances
  the fork-point SHA.

The SHA half is the **squash-safe anchor**: the parent-tip commit this
worktree's own commits sit on. Because replays cut at the anchor
(`git rebase --onto <newParent> <anchor> <branch>`), a parent that
squash-merged is excluded by construction — its commits sit below the anchor —
with no patch-id guessing.

## Inferred stacks

A worktree whose recorded base names another live worktree's branch is stacked
on it. Chains of records form a stack: the TUI groups the members into one
section (tree spine in the gutter, AI-generated title on the header), each
member diffs/syncs against its parent instead of trunk, and the AI summary
describes only what that member adds. Forks are fine — two worktrees based on
the same parent render as parallel lanes.

Stack identity is the root member's branch. When the root lands and its
worktree is cleaned, the first child re-roots the stack; section folding and
automation pauses keyed to the old root start fresh.

## Restacking

`wt restack` (CLI) or `R` (TUI) realigns a whole stack after parents
move — and works identically on a standalone worktree, which resolves
as a one-member chain rebasing onto its recorded base or plain trunk
(local-only branches are rebased but never pushed). Press it anywhere;
it does the right thing for the shape under the cursor:

1. **Fetch** origin.
2. **Reconcile** records against landed PRs: a member whose parent's PR merged
   (or whose parent branch is gone everywhere) is reparented onto the nearest
   surviving ancestor, falling back to trunk — anchor preserved, so the next
   replay stays squash-safe.
3. **Replay** each member onto its (possibly rewritten) parent in its own
   worktree, parents before children; force-with-lease push; retarget the PR
   base to match. A member reconcile observed **landed** (a merged parent that
   is itself still a live, uncleaned worktree) is skipped, not replayed —
   replaying it would re-apply its squash-merged commits onto trunk and
   force-push, resurrecting the landed branch. Landed members are `c`'s job.
   So pressing `R` on a surviving sibling is safe while a merged member is
   still on disk (and the `stack.parent_merged` automation's clean-then-restack
   is unaffected by the order the two land in).

Cleaning a merged member (`c`, or the `wt.merged`/`stack.parent_merged`
automations) reparents its children automatically when the branch is deleted —
onto the deleted branch's own recorded base, anchors kept — so the stack heals
itself as PRs land; the replay stays an explicit `R`/`wt restack`.

Restacks lock **per chain**, not globally: the engine takes every member's
per-slug flock (the same locks creates/destroys use) for the duration, so
disjoint stacks — and unrelated standalone worktrees — restack concurrently,
while two operations touching the same worktrees (a second `R`, a CLI run, a
destroy) refuse with "busy". While the locks are held every member row shows
the restack glyph (accent sync icon). Once you (or `/restack`) start the
resolving rebase in the bailed worktree, the same glyph shows in warn until
that rebase finishes or aborts — the bail itself leaves the tree clean (see
below), so right after it the row shows the red conflict triangle instead.

Conflicts are never auto-resolved by the **engine**: it aborts the rebase,
leaves a `backup/restack-*` ref at the old tip, and names the failing branch
(exit 3 at the CLI). From the TUI (`R`, or an auto-restack) the bail hands off
automatically — the bundled `/restack` skill, which knows the full recovery
loop, is injected into the failing worktree's harness session (cold-started if
needed) with the bail context. From the CLI, run it yourself or resolve by
hand, then re-run; the anchor logic self-heals around hand-rebases. Leftover
backups: `wt restack prune-backups`.

The `stack.parent_merged` automation trigger paired with `builtin:restack`
makes the whole loop hands-off: when a parent merges under open members, wt
cleans the landed worktrees and restacks the survivors (see
[automations.md](automations.md)).
