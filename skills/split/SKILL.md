---
name: split
description: >-
  Carve the current holistic branch into a stack (or parallel lanes) of small
  draft PRs. Decides the shape, verifies each slice builds, writes a wt stack
  manifest, and materializes worktrees + draft PRs. User-initiated via /split.
targets:
  - '*'
argument-hint: [optional notes, e.g. "keep the schema slice separate"]
user_invocable: true
---
# Split Into a Stack

Turn one validated holistic branch into several small, reviewable PRs.

## Current State

!`WT="$HOME/.wt/bin/wt"; [ -x "$WT" ] || WT=wt; "$WT" stack context`

## Mental model

You already implemented and reviewed the whole change on this branch. The stack
is a **derived view** of that validated diff, sliced for human review. Don't
re-review or re-test the feature as a whole here — slice it, verify each slice
compiles, and ship small draft PRs. After the split, the stack becomes the
source of truth and the holistic branch is archived as a tag.

Size philosophy: ship small, reviewable slices — aim for ≤3 files / ~150
production LOC each (advisory targets, not gates), and bias smaller when unsure.

## Preconditions

- **Fresh base, then resolve it — don't assume.** The Current State above runs
  `git fetch` first, so main is current, and resolves the base itself: a
  CONFIRMED **recorded fork base** (`wt new --base` persists the parent;
  `wt base <slug>` shows it; `wt stack context` verifies HEAD is on top of it)
  wins and IS the answer — case (b) below with that parent, proceed without
  asking (the changed-file inventory already diffs against it; note a recorded
  base outranks "HEAD is on top of origin/main", which is also true whenever the
  parent chain was just rebased onto main). Without a confirmed record, if
  `base status` shows HEAD is NOT on current `origin/main`, STOP and **ask
  the user which case it is** (the diff and every slice's `base` depend on
  the answer):
  - **(a) main moved / a parent PR merged** → have them `git rebase origin/main`,
    then re-run /split. Base = `main`.
  - **(b) intentionally stacked on an unmerged parent branch** → base = that
    parent; each root slice's `base` is the parent branch (not `main`), and the
    slices stack on the parent's PR. `/restack` later rebases the whole thing onto
    main once the parent lands.

  Never slice a diff computed against stale main — it folds already-merged work
  into your slices.
- On a feature branch (not `main`), working tree clean, work committed.
- If `wt` lacks the stack commands yet, you can still produce and save the
  manifest JSON and present the plan; report that materialization needs
  `wt stack apply`.

## Steps

### 1. Inventory the diff
List changed files vs base and their **production** size (files + LOC, excluding
tests/snapshots/generated/lockfiles). Prefer `wt size --json`; fall back to git.
Note the `holisticBranch`, `holisticSlug`, the issue id, and this conversation's
session id (for `holisticSessionId`, so slices can find this convo later).

**See renames and deletions as two paths, not one.** A `--stat` (and `wt size`)
view collapses a rename to a single `{old => new}` line, which reads as one file
to assign and hides the delete-half. Always cross-check with
`git -c core.quotePath=false diff --name-status -M <base>...HEAD` (quotePath off so
non-ASCII paths come back literal): a `D` is a deletion you must claim, an
`R old new` is a deletion (`old`) **and** an addition (`new`) that BOTH need a
slice. Missing the `old` half is the classic split bug — it lingers from base and
red-CIs the one slice that removes what it depends on. (`wt stack apply`/`plan`
now hard-fail on an unclaimed path, but author it right and skip the round-trip.)

### 2. Build the dependency picture
For each changed file, find which other changed files/symbols it imports or
references. Use the codebase's layering as the topological backbone (e.g. a
layered repo: core → domain → UI). Keep each `X.spec.ts` with its `X.ts`.

### 3. Propose the slices
- **Targets are advisory:** aim for ≤3 files / ~150 production LOC per slice; go
  smaller when unsure. They are suggestions, not gates.
- **File-level is the default.** A changed file belongs wholly to one slice
  (`files: [...]`). Reach for hunk-level (below) only when whole-file ownership
  would force an otherwise-clean stack into one indivisible blob.
- **A rename and a deletion must be claimed, in full.** Every changed path the
  inventory surfaced — including each `D` deletion and BOTH halves of an `R`
  rename — must appear in exactly one slice's `files`. For a relocation (move a
  function + its test, split a module), list the **old** (deleted) path in the
  slice that removes the symbol it depends on, or any earlier slice; list the
  **new** path wherever its content belongs. They needn't be the same slice, but
  neither may be dropped. `wt stack plan`/`apply` enforce full coverage and name
  any unclaimed path, so a miss is caught before PRs — but get it right up front.
- **Maximize parallelism.** Two slices that are file-disjoint AND
  symbol-independent become **parallel lanes** (each branches off `main`,
  `dependsOn: []`). Slices on a dependency chain **stack** in order. If two
  candidate lanes touch the same file, serialize them into a stack instead
  (avoids merge-order conflicts).
- **Hunk-level partition (when file-level would block a clean split).** When a
  single file's changes legitimately serve different slices (e.g. a fixture
  stub that belongs in an early "add the field" slice and the behavior that
  reads it in a later slice, or a plumbing hunk vs a gate hunk in one handler),
  a slice can own only PART of a file: `partials: [{ file, hunks: [<id>...] }]`.
  Get the canonical ids with `wt stack hunks [--holistic <branch>] [--json]
  <file>...` (do NOT hand-derive them — wt content-hashes each hunk and
  reconstructs against the same ids). Rules wt enforces at `apply` (it bails
  loudly otherwise, so honor them up front):
  - Every holistic hunk of a partial file is owned by **exactly one** slice
    (full coverage, no overlap).
  - The slices owning a given partial file must form a **single dependency
    chain** — one of them transitively `dependsOn` all the others, so that
    deepest slice's commit carries the whole file. Never split one file's hunks
    across **parallel lanes** (no tip would hold the complete file).
  - A file is whole (`files`) **or** hunk-split (`partials`) across the stack,
    never both; all its hunks come from one source.
  - **Two edits within ~3 lines of each other coalesce into ONE git hunk** at
    the default context. If they genuinely belong in different slices, drop the
    diff context: `wt stack hunks --unified 0 <file>` lists the finer hunks, and
    you MUST then pin `hunkContext: 0` at the manifest top level (the ids are
    content-hashed at that context, so listing and `apply` must agree — a
    mismatch fails loudly). If even `-U0` can't separate them (truly interleaved
    lines), it's a genuinely indivisible unit — don't force it.
- **Don't mutilate an indivisible unit.** Before declaring a file indivisible,
  check whether its hunks separate cleanly (above). If they genuinely don't (one
  cohesive function, or interleaved changes git can't hunk-split), the file gets
  its own slice with `oversized: true` and an `oversizedReason` — not a fake
  intra-file seam.

### 4. Verify the ordering compiles
The holistic branch already typechecks; the risk is a slice that references a
symbol only added in a later slice (much more likely once you hunk-split — a
whole-file stack compiled per slice for free, a hunk split can take a body
without its import). Trace the imports: each slice's new files may reference only
earlier slices or the base. For a hard check, let `wt stack apply --verify` do it
(step 6) — it reconstructs each slice's tree and typechecks it before opening any
PR. Do NOT typecheck inside the slice worktrees: `wt stack apply` makes them
install-free (a slice == a light worktree, basically a branch checkout), so they
have no `node_modules`. Per-slice CI is the backstop. If a prefix wouldn't
compile, adjust the partition (pull the dep into an earlier slice, or merge two)
and re-trace. Tests ride with their code.

### 5. Present and get approval
Show the plan as a table: slice / branch / files / prod LOC / lane-or-depends-on
/ oversized. Let the user merge, reorder, rename, or relabel. Use the branch
naming `<prefix>/<issue>-<NN>-<slug>` for stacked slices (2-digit ordinal),
`<prefix>/<issue>-<slug>` for parallel lanes, where `<prefix>` is your wt
`[branch] prefix`. Don't create issue-tracker subissues — the issue id in each
branch auto-associates every slice PR to the issue.

### 6. Materialize
Optionally validate first with `wt stack plan --from <file>` (strict ingest, no
PRs) and eyeball `wt stack status <stackId>`. Then `wt stack apply --from <file>`.
**Never write wt's state file (`state.json`) yourself** — wt owns its state and
validates the manifest strictly on ingest, so a malformed manifest fails loudly
instead of materializing a wrong stack. apply creates an install-free worktree
per slice, reproduces each slice as one commit, pushes, opens a **draft PR** with
the correct base, seeds wt's explicit parent, and archives the holistic branch as
`refs/tags/<stackId>-holistic`. For a bigger stack (especially a hunk-split one)
where a red CI across N PRs would sting, pass `wt stack apply --verify`: it
reconstructs each slice's tree in a throwaway worktree (deps symlinked from the
holistic worktree) and runs the configured `[stack] verify_command` against each
before opening anything. Needs that command set in config; it's a fast early
gate, not a CI replacement (deps are symlinked wholesale, so a slice importing a
package the stack itself adds can still pass). Default relies on per-slice CI —
fix whatever it surfaces.

### 7. Write the PR bodies
Do this immediately at materialize (don't leave wt's stubs sitting). Each body is
two parts:
- **Intent prose** (top): clear, intent-focused PR prose, framed by the feature —
  one or two sentences on what THIS slice does and how it serves the whole, so a
  reviewer of a 1-file slice knows why it exists. The slice with a real observable
  can show a tiny before/after. The oversized slice carries its `oversizedReason`
  ("single unit, N lines, not splittable"). **Never mention the holistic
  branch/PR** — it's a closed implementation detail.
- **Stack section** (bottom), generated once by
  `wt stack section <stackId> <thisPr> "<feature label>"`. Header is
  `Stack: **<feature>**` (only the title bold). The list is **bare `#refs`** (no
  titles — GitHub expands each to its live title + merge/closed status) with a 👈
  marking this PR. A linear stack renders as the flat numbered list; a non-linear
  one (parallel lanes, a mid-stack fork) renders as a nested bullet tree —
  nesting = stacks on, siblings = parallel. No stack-on-stack annotation: the
  external parent is deliberately not mentioned. It is NOT maintained *as slices
  land* — GitHub renders status live. The one time it goes stale is when the
  slice **set** changes (a mid-stack re-split adds/removes PRs); regenerate every
  affected body then (see below).

Assemble `prose + "\n\n" + section` per slice and `gh pr edit <pr> --body-file`.
The blank line between prose and the section's `---` is REQUIRED: markdown
parses `text\n---` as a setext heading, turning the last prose paragraph into
a giant bold H2 (`wt stack section` also emits a leading blank line as a backstop).

### 8. Report
Print the stack: each slice's branch + draft PR URL, lanes vs chain, and any
oversized-flagged slice. Remind the user the holistic worktree can be `wt rm`'d
and the old single PR (if any) closed as superseded.

## Manifest shape

```jsonc
{
  "stackId": "eng-5182", "issue": "ENG-5182",
  "holisticBranch": "<prefix>/eng-5182-...", "holisticSlug": "eng-5182-...",
  "holisticSessionId": "<this session id>",
  "archivedTag": "refs/tags/eng-5182-holistic",
  "limits": { "files": 3, "prodLines": 150, "hard": false },
  "engine": "stack",
  // optional, omit for the default (3). Set 0 ONLY if you listed hunks with
  // `wt stack hunks --unified 0` to separate coalesced edits — listing and
  // apply must use the same context or the content-hashed ids won't match.
  "hunkContext": 0,
  "slices": [
    { "id": "s1", "ordinal": 1, "title": "Builder context + read tools",
      "branch": "<prefix>/eng-5182-01-read-tools", "base": "main",
      "dependsOn": [], "files": ["..."], "pr": null, "status": "planned",
      "oversized": false }
  ]
}
```

A hunk-level slice carries `partials` (and may have an empty `files`). The two
slices below split `createMessage.ts` by hunk — note the chain (`s4` dependsOn
`s2`), so `s4`'s commit reconstructs the whole file:

```jsonc
{ "id": "s2", "ordinal": 2, "base": "s1", "dependsOn": ["s1"],
  "files": ["User.ts"], "partials": [{ "file": "createMessage.ts", "hunks": ["a1b2c3d4e5f6"] }],
  "pr": null, "status": "planned", "oversized": false },
{ "id": "s4", "ordinal": 4, "base": "s3", "dependsOn": ["s3"],
  "files": ["channels.ts"], "partials": [{ "file": "createMessage.ts", "hunks": ["9f8e7d6c5b4a"] }],
  "pr": null, "status": "planned", "oversized": false }
```

## Re-splitting a live slice (mid-stack)

When an already-open slice in a live stack turns out oversized, you don't
re-derive the whole stack (earlier slices may be merged). Reshape just that
slice in place with `wt stack split`, which replaces one open slice with N
sub-slices and re-threads its descendants onto the new tip.

The sub-slices partition the **original slice's branch**, not the holistic
branch — so if the split needs a refactor (e.g. carving a 500-line file into
modules), commit that refactor onto the slice's branch FIRST. That branch
becomes the `source` the sub-slices reproduce their files from.

**Two cautions specific to re-splitting** — both turned a clean re-split into a
multi-step mess in practice:
- **Order each sub-slice to compile, and let `--verify` prove it.** Every
  sub-slice's prefix must typecheck on its own. The classic trap: a slice that
  extends a discriminated union (adds a variant) ordered BEFORE the exhaustive
  consumer that has to handle it — a `switch`, or a helper whose parameter is
  `Extract<Union, {…}>` (adding a member it doesn't handle is a type error).
  Co-locate the union extension with its exhaustive consumer, or sequence the
  consumer's slice first. Don't reason it through; make `--apply --verify`
  (step 4) machine-check it before any PR opens.
- **Tearing a materialized sub-slice back down is hazardous — avoid it.** If a
  partition turns out wrong AFTER materialize, prefer reshaping forward over
  delete-and-redo. If you must redo: (a) deleting a slice branch that is the
  live PR base of a descendant **auto-closes that descendant PR**, and GitHub
  won't reopen it while its base branch is gone — so retarget/rebuild descendants
  onto the new tip BEFORE deleting any old branch; and (b) give the redo **fresh
  branch names** — reusing a name whose prior PR is closed is confusing even
  though `wt stack apply` no longer adopts a closed PR.

1. **Refactor + commit on the slice branch** (judgment, local). Carve the files
   so each sub-slice is a clean partition. Verify it typechecks in a dep-having
   checkout. The slice's branch tip now holds the final shape.
2. **Write a fragment** — a JSON array (or `{ "into": [...] }`) of sub-slice
   specs, each `{ id, title, branch, files[], partials?, oversized?,
   oversizedReason? }`. The files (and hunks, for any `partials`) across all
   sub-slices must partition the slice branch's diff — here the `--holistic`
   for `wt stack hunks` is the slice's own `source` branch, not the stack's
   holistic branch.
3. **Preview**: `wt stack split <stackId> <sliceId> --from frag.json --plan`.
   Confirm the new chain + which descendants re-thread onto the last sub-slice.
4. **Reshape + materialize + replay**: `wt stack split <stackId> <sliceId>
   --from frag.json --apply --verify` chains the whole mechanical path — rewrite
   the manifest, materialize the new sub-slice branches/PRs from the recorded
   `source` (skipping everything already open/merged), then rebase the
   re-threaded descendants onto the new tip and retarget their PR bases. **Always
   pass `--verify`**: it reconstructs each new sub-slice's prefix in a throwaway
   dep-having worktree and typechecks it, aborting BEFORE any PR opens if one
   fails to compile — the cheap gate that catches a mis-ordered partition (the
   union-ordering trap above) instead of shipping it as open PRs you then have to
   tear down. (Run it without `--apply` to do it by hand: reshape, then `wt stack
   apply --verify`, then `wt stack rebase`/`R`. A mid-replay conflict drops you
   into `/restack`.)
5. **Regenerate the stale stack sections.** The split changed the slice *set*,
   so the bottom stack section of every **new sub-slice** AND every **re-threaded
   descendant** now lists the old set (the superseded PR, none of the new PRs).
   `split` warns with the exact list. Re-run
   `wt stack section <stackId> <thatPr> "<feature>"` for each and
   `gh pr edit <pr> --body-file` (keep each body's intent prose, swap only the
   section). Slices that didn't re-thread are untouched.
6. **Retire the old slice**: the split slice is gone from the manifest but its
   branch/PR linger (the branch was the materialize `source`, needed through
   step 4's apply). Then `gh pr close <oldPr> --delete-branch --comment
   "superseded by re-split"`.

`wt stack split` refuses to re-split a `merged` slice. Inserting/dropping a slice
mid-stack are the natural siblings of this op but aren't built yet.

## Notes
- Don't re-review per slice — the holistic branch was already reviewed.
- Maintenance (rebase on main, conflicts, landing) is `/restack`, not this skill.
- Design rationale + open questions: the wt repo's `docs/stacking-workflow.md`.

## User Instructions

$ARGUMENTS
