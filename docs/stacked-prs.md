# Stacked PRs

A stacked PR is just a branch based on another branch. wt keeps that
relationship as one small per-worktree record and derives everything else from
it — there is no managed stack state, no registration step, and nothing to
keep in sync.

## Stacks and sections

Sections own the vertical axis; a stack is a **relationship between rows inside one**, not a place of its own. A stack used to render as its own pseudo-section, which overrode whatever section the human had filed each member in — three stacks produced three headers all literally named `stack`, mutually indistinguishable, each displacing a real section name, and stacked worktrees could not be filed at all.

Members therefore keep their own `section` record and a stack sorts as one contiguous **unit**, slotted by its most urgent member so `sort = "status"` can't interleave unrelated rows through a spine. `J`/`K` from any member moves the whole block (the unit's slot is the root's manual order), and filing a member via `l` — or `wt section mv` — moves the whole stack unless `--only`.

## The rail

The stack rail is the `tree(1)` / `git log --graph` idiom, and each of its three dimensions answers exactly one question:

- **Column = depth.** The rail is an indent, so siblings share a column and a chain steps right: a fan reads as a fan without a legend.
- **Glyph = position among siblings.** `├` when another sibling follows, `└` for the last one, `┌` on the row that tops the spine, `│` continuing an ancestor's column past rows that belong to a deeper branch.
- **Color = lane.** Which parallel branch of a fork a row descends from, which is the one thing a single column of glyphs genuinely can't express.

```
┌   Chain root lays the schema        ┌   Fan root extracts the client
├   Chain middle adds the reader      ├   Fan left adds the writer
│└  Chain leaf wires the cache        └   Fan right adds the reporter
└   Chain unstarted
```

There are deliberately no `01`/`02` ordinals: numbering a fork's children asserts a merge order that doesn't exist. If ordinals are ever wanted, merge **edges** are the thing that actually encodes order.

**The rail describes the sub-tree on screen, not the stack.** Glyphs are laid out per contiguous rendered group (a section in the list, the member list in a folded-section summary) and in draw order, by `spineLayout` in `core/stack-layout.ts` — `buildStackIndex` supplies structure (parent, depth, lane), never glyphs. That's what makes the rail honest rather than decorative: a member whose parent is folded away or filed elsewhere tops its own spine at column 0 instead of floating one column in above an empty gutter, a member with nothing else from its stack alongside it draws nothing at all, and `├` vs `└` always agrees with what is actually above and below. The gutter is sized from the cells that get drawn, so a stack whose root lives elsewhere never reserves a column it doesn't use.

The glyph used to come from a node's own CHILD count (`┯` where a stack forked, `┌` for a root with one child). That read as box-drawing but didn't join up — a root's connector was never drawn at all, so `┌` and `┯` were unreachable and every spine hung off nothing above it.

**Splitting a stack across sections is legitimate**, not a mistake to reconcile: finished parents awaiting verification and their unstarted children genuinely belong in different buckets. A member whose parent sits elsewhere draws no rail up to it and carries a dim `→ <parent's section>` reference instead, so the relationship survives the split. The reference names the SECTION, not the parent row: the parent is off-screen by definition, and if its section is folded the row isn't rendered at all — a section name is somewhere the reader can actually go (a divider on screen, or a header to unfold). Which parent stays a details-pane question. The reference shrinks into whatever width is left above a 24-cell floor for the row's own label, and is dropped below that rather than truncating two siblings of one parent to the same prefix — identity beats relationship when there isn't room for both.

## The base record

Every worktree carries a **fork base**: the branch it's based on, plus the
fork-point SHA (`baseBranch` / `baseSha` in wt's state file). It's written
three ways:

- `wt new` — records the parent and the fork point at creation. Written for
  **trunk** forks too, not just `--base <ref>` ones. A trunk fork is not
  stacked on anything and every reader normalizes `baseBranch == trunk` to
  "no parent", so this changes no grouping — but the SHA has a second
  consumer outside stacking: the vacuous-containment guard in
  `branchIsMerged` measures "has this branch got commits of its own"
  against it, and with no record that guard fails open and calls an
  unstarted branch merged. `merged` closes GitHub issues and feeds the
  clean sweep, so an absent anchor there is not a stacking gap, it's a
  correctness one.
- `wt base set <slug> <ref>` / the TUI's `b` picker — backfill or change it by
  hand (record only; nothing is rebased).
- `wt base clear <slug>` returns the recorded parent to trunk while retaining
  the existing `baseSha`. That anchor still marks where the branch's own
  commits begin, including after a squash merge. Setting a different explicit
  base computes a new merge-base anchor.
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
section (tree spine in the gutter, harness-generated title on the header), each
member diffs/syncs against its parent instead of trunk, and the generated summary
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

**How the parent's branch gets deleted matters for the CHILD PRs.** With the
repo's "automatically delete head branches" setting, GitHub retargets open
child PRs to the merged parent's base (they survive and stay open). Deleting
the branch by API — which is what `gh pr merge --delete-branch` does — makes
GitHub **close** the child PRs instead, and a PR closed this way is
unrecoverable: its base can't be edited and it can't reopen once the base ref
is gone. The restack still replays the child branches fine; the engine detects
the closed-by-base-deletion case during its retarget pass and raises an
attention line telling you to open a fresh PR. Prefer the repo setting (or the
web UI's post-merge "Delete branch" button, which also retargets) over
`--delete-branch`.

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
loop, is sent to the failing worktree's harness session (cold-started if
needed) with the bail context. From the CLI, run it yourself or resolve by
hand, then re-run; the anchor logic self-heals around hand-rebases. Leftover
backups: `wt restack prune-backups --days <n>` removes recognized backups
older than the cutoff; omitting `--days` removes all recognized restack
backups. Unrecognized refs under `backup/` are kept.

The `stack.parent_merged` automation trigger paired with `builtin:restack`
makes the whole loop hands-off: when a parent merges under open members, wt
cleans the landed worktrees and restacks the survivors (see
[automations.md](automations.md)).
