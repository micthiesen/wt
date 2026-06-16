# Small-stacked-PR workflow — design & rationale

> Living design doc. When something in this workflow needs tweaking, read this
> first for full context, then update it (decisions, what works, what to change).
> Last updated: 2026-06-09.

Full design conversation (deep context, the back-and-forth that produced this):
`/Users/michael/.claude/projects/-Users-michael-Code-client-wt-eng-5182-metric-builder-tools-and-v2-instructions/9323e329-e087-59be-a2cf-7ffd88e9ab4e.jsonl`
(read directly or via the `/history` skill).

---

## The problem this solves

Michael gets strong social pushback when a PR "feels big." It's a **vibe**, not a
written rule: a coworker reacts when a PR spans more than a few files or just
feels heavy. Asking for the line produces no clarity — everyone agrees small
stacked PRs are good, but nobody will define where to split. The reaction
secretly tracks LOC + file count even though people won't admit it. Even
well-split ~400-prod-line PRs have drawn it. It's a real, recurring stressor.

**Conclusion that drives everything below:** there is no "correct line" to find,
and the team won't hand one over. So stop hunting theirs. Set our own, stricter,
mechanical line and follow it predictably. The relief is removing the *guessing*,
even if the self-imposed rule is stricter than strictly necessary. Optimize to
stay so far under the radar the vibe never fires — not for "correct" granularity.

This is deliberately willing to split **past the point where it helps
comprehension**, because the binding constraint is the social reaction, not
reviewability. Over-splitting is cheap; an oversized PR is expensive (pushback +
worse review). The payoff is asymmetric, so bias hard toward small.

---

## Core mental model

The unit of **implementation** and the unit of **review** are different
granularities. Don't conflate them.

- **Implement holistically.** Build + `/ultracheck` the whole feature on one
  branch. That's where correctness is established, once, over the whole change.
  This is what `/start` already does; it does NOT change. (Working in tiny units
  from the start is impractical — a Linear issue is a holistic feature/bug, and
  ultracheck wants the whole picture.)
- **The stack is a derived view.** Splitting is **post-hoc**: carve the validated
  diff into small slices for human review.
- **Ownership of truth flips once.** Holistic branch is the source of truth until
  the split; at the split, the **stack becomes the source of truth** and the
  holistic branch is archived as a tag. After that, edits happen on slices and
  restack upward. (This is what avoids the classic "do I fix the slice or the
  holistic branch" stacked-PR nightmare.)

ultracheck runs **once** on the holistic branch. Verification is a cheap
typecheck in a dep-having checkout (not the install-free slice worktrees) plus
per-slice CI, never a re-ultracheck.

---

## Locked decisions

- **Budgets are advisory, not gates.** Target ≤3 files / ~150 production LOC per
  slice (exclude tests, snapshots, generated, lockfiles). Suggestions for the
  model; go smaller when unsure.
- **Explicit-only.** Stack relationships come from an explicit manifest, never
  inferred from reflog or PR base. (We're gutting wt's reflog heuristics.)
- **File-level slicing** by default. Intra-file (hunk) splitting only when one
  file is over budget and has genuinely independent additions; otherwise flag it
  oversized rather than butcher it.
- **Tests ride with their code** (`X.spec.ts` with `X.ts`). Don't defer tests to
  a follow-up PR — that trades size-anger for "where are the tests" anger.
- **Maximize parallelism.** File-disjoint + symbol-independent slices become
  parallel lanes off `main`; dependent slices stack in order. Lanes that touch
  the same file get serialized into a stack to avoid merge-order conflicts.
- **Indivisible over-budget unit is fine.** Flag it (`oversized` + reason) and
  note "single unit, N lines, not splittable" in the PR body. Reviewers accept
  "irreducible" far more readily than "huge."
- **Draft PRs are created at split time** (more knowledge there than later).
- **No Linear subissues.** Slice branches carry the issue id, so every slice PR
  auto-associates to the one Linear issue.
- **Branch naming:** `michael/<issue>-<NN>-<slug>` (2-digit ordinal) for stacked
  slices; `michael/<issue>-<slug>` (semantic) for parallel lanes.
- **Slice == worktree.** Each slice gets its own light worktree (created
  install-free, so it's basically a branch checkout + a pinned `.sst/stage` file
  with no deploy cost). We considered decoupling slices from worktrees; rejected —
  a split-brain "some slices have worktrees, some don't" is more confusing than
  six cheap worktrees. Consequence: a slice worktree has no `node_modules`, so
  **verification runs in a dep-having checkout** (the holistic worktree or a
  scratch one), never inside the slice worktrees; per-slice CI is the backstop.
- **wt owns its state; the skill never writes `state.json`.** `/split` emits the
  manifest JSON and hands it to `wt stack apply --from <file>`. wt validates
  **strictly and loudly** on ingest (unlike the lenient read-path coercion in
  `parseManifest`), so a malformed/typo'd manifest fails fast instead of
  materializing a subtly-wrong stack. No skill ever edits `~/.cache/wt/state.json`
  directly — that boundary is a CLI, not a private file format.
- **Base detection asks, never guesses.** `/split` fetches first (main is always
  current). A CONFIRMED recorded fork base (`wt new --base` persisted it and HEAD
  is on top of it) is an explicit prior answer, not a guess — it resolves case (b)
  without asking and outranks "HEAD is on top of origin/main" (also true whenever
  the parent chain was just rebased onto main). Without one, if HEAD isn't on
  current `origin/main`, it asks: (a) parent merged / main moved → rebase onto
  main (base = main); or (b) intentionally stacked on an unmerged parent → base =
  that parent branch, slices stack on its PR, `/restack` rebases onto main once
  the parent lands. Never silently rebase or split stale.
- **PR bodies = intent prose + a generate-once stack section.** `/split` writes
  bodies at materialize (improvement 3 — never leave wt's stubs). Top: intent
  prose per CLAUDE.md's PR rules, framed by the feature so a 1-file slice's
  reviewer knows why it exists; the oversized slice carries its `oversizedReason`.
  Bottom: a "Stack" section, header `Stack: **<feature>**`, then bare `#refs` (no
  titles — GitHub expands each to its live title + merge status) with a 👈 on this
  PR. Generated once by `split/scripts/stack-section.sh` from
  `wt stack status --json`. It is NOT maintained — GitHub renders status live, so
  it never needs updating as slices land. **Never
  mention the holistic branch/PR** (closed implementation detail).
- **Verify is opt-in; CI is the default gate.** Rely on per-slice CI and fix what
  it surfaces. `wt stack apply --verify` reconstructs each slice's materialized
  tree (base + its ancestor-closure) into a throwaway worktree at the holistic
  base, symlinks the holistic worktree's deps in, and runs `[stack].verify_command`
  against each before opening any PR — worth it for a bigger stack, skippable for a
  small one. It's a fast early gate, not a CI replacement: deps are symlinked
  wholesale (a slice importing a package the stack itself adds can pass even though
  its PR won't have it), and it assumes a single-root project. Hunk-level slicing is
  the reason it exists — a whole-file stack always compiled per slice; a hunk split
  can take a body without its import.

---

## Architecture: state vs algorithm vs judgment

Three layers, split so no layer owns something it shouldn't:

- **wt owns STATE.** A per-feature **stack manifest** is the single authoritative
  description of the stack shape, stored in wt state (`~/.cache/wt/state.json`,
  new top-level `stacks` map). Full local control — this is the one source of
  truth, which is the whole point after years of "heuristics vs GH PR state"
  ambiguity.
- **`@kitlangton/stack` is a driven ENGINE, not a state owner.** Used only for the
  genuinely hard part: squash-safe restack (anchor merge-base + cherry-pick
  replay + force-with-lease push + PR-base retarget) and merge-queue landing
  (`stack merge --auto --through`). Driven by explicit `stack track` calls
  generated from the manifest. Its `.git/stack/state.json` is treated as a
  **regenerable projection**, never read for truth.
- **Claude skills own JUDGMENT.** `/split` decides the shape and writes the
  manifest; `/restack` drives maintenance and resolves conflicts.

Keep a thin `RestackEngine` seam in wt around the `stack` calls, so the engine
can later be reimplemented inside wt and the dependency dropped without touching
the skills. Using `stack` now is the low-regret choice; the only thing that's
hard to rebuild is the anchor+cherry-pick replay.

### Why this division resolves the old pain
The recurring complaint was two competing sources of truth (reflog/merge-base
heuristics vs GitHub PR base state). Here there is exactly ONE authoritative
store — wt's manifest — and stack's state is downstream output that wt rewrites
before every operation. A `wt stack status` reconcile step surfaces drift rather
than silently trusting either side.

---

## The manifest

Lives in wt state, one per feature, keyed by `stackId`.

```jsonc
{
  "stackId": "eng-5182", "issue": "ENG-5182",
  "holisticBranch": "michael/eng-5182-...", "holisticSlug": "eng-5182-...",
  "holisticSessionId": "<convo id>",          // slices reach the full convo via /history
  "archivedTag": "refs/tags/eng-5182-holistic",
  "limits": { "files": 3, "prodLines": 150, "hard": false },   // ADVISORY
  "engine": "stack",
  "slices": [
    { "id": "s1", "ordinal": 1, "title": "Builder context + read tools",
      "branch": "michael/eng-5182-01-read-tools", "base": "main",
      "dependsOn": [], "files": ["..."], "pr": null, "status": "planned",
      "oversized": false }
  ]
}
```

`dependsOn: []` + `base: "main"` = parallel lane. A `dependsOn` chain = stack.
`status` ∈ planned | open | merged. The holistic origin is tracked separately so
wt can show it as a distinct node and slices can find the source conversation.
Optional `source` (a branch) overrides where a slice reproduces its files from
at materialize (default `holisticBranch`); set by `wt stack split` so a re-split
slice's sub-slices partition the original slice's branch, not the holistic.

---

## End-to-end pipeline

```
/start        implement holistically + ultracheck (UNCHANGED, project skill)
              └─ size advisory lives in CLAUDE.md, not the skill
/split        post-hoc splitter (personal skill):
                diff → dep graph → propose file-level slices (max parallelism,
                advisory budget) → VERIFY each cumulative prefix typechecks →
                write manifest → approve → `wt stack apply --from <manifest.json>`
wt stack apply   materialize: worktrees + per-slice commit (git checkout
                 <holistic> -- <files>) + push + draft PR (correct base) +
                 `stack track` + record PR in manifest + archive holistic as a tag
                 (the wt list derives parent/order/spine from the manifest)
/done         stack-aware in spirit (via CLAUDE.md): if a manifest exists, ship
              the whole stack, not one PR (project skill, NOT modified)
/restack      maintenance (personal skill): drives `wt stack rebase`; on a
              clean engine bail, resolves conflicts in-tree, typecheck-gates,
              force-with-lease pushes, re-syncs; updates the manifest
```

---

## Where each piece lives

The skills + CLAUDE.md guidance are **generated** by `rulesync` from canonical
source in `~/.dotfiles/.rulesync/`. Edit the source, never the generated `ai/`
output, then run `bash ~/.dotfiles/scripts/rulesync.sh` to regenerate `ai/` and
stow to every harness (Claude Code via `~/.claude`, Codex via `.agents`→`.claude`,
OpenCode via `.claude/skills`).

- **CLAUDE.md guidance** — source `~/.dotfiles/.rulesync/rules/CLAUDE.md` → `## PR
  Size & Stacking` (generates `ai/.claude/AGENTS.md`; CLAUDE.md symlinks to it).
  Holds the size philosophy + the re-homed `/start`/`/done` behavior.
- **`/split`** — source `~/.dotfiles/.rulesync/skills/split/` (SKILL.md + scripts/).
- **`/restack`** — source `~/.dotfiles/.rulesync/skills/restack/` (SKILL.md + scripts/).
- **`/improve-stacking`** — source `~/.dotfiles/.rulesync/skills/improve-stacking/`.
  Meta-skill: describe a friction in plain words, it routes the fix to the right
  component (wt code, a skill, CLAUDE.md, or this doc), keeps the pieces coherent,
  and logs it.
- **wt implementation** — built and shipped in `~/.wt` (the `prompt.txt` brief
  was consumed and deleted). The contract now lives in the code + this doc:
  manifest in `core/wtstate.ts` (`stacks` map); strict ingest in
  `validateStackManifest`; layout in `core/stack-layout.ts`; materialize/rebase
  in `core/stack-ops.ts`; `wt stack apply|plan|status|rebase` + `wt size`.
  Key contracts for the skill:
  - **Ingest is `wt stack apply --from <file>`** (or `wt stack plan --from <file>`
    to validate + ingest without materializing). STRICT validation (separate from
    the lenient `parseManifest` read path): unknown keys, missing
    id/branch/files/ordinal/base, dangling/self `dependsOn`, duplicate id/branch,
    `oversized` without reason all ERROR loudly (all problems at once, non-zero
    exit). The skill never writes `state.json`.
  - **`base` accepts any branch string.** Trunk base → off `origin/main`, PR vs
    trunk. A sibling slice id or an external parent-PR branch → off that branch,
    PR vs it, engine-tracked. `isTrunkBase` (not "is this a root") gates the
    trunk-only behavior, so a stack rooted on an unmerged parent materializes
    correctly.
  - **No per-slice build gate.** Slices are install-free (no `node_modules`), so
    `apply` cannot and does not typecheck them. Verification is the skill's job,
    run BEFORE materialize in a dep-having checkout; per-slice CI is the backstop.
- **This doc** — `~/.wt/docs/stacking-workflow.md` (edit in place; not generated).

### Re-homing note (important constraint)
The user's `/start` and `/done` are **project skills** (in client-app) that take
precedence over personal skills, so they are NOT modified. The size advisory
(after holistic work, report size and suggest `/split` if over budget) and
`/done`'s stack-awareness therefore live as **CLAUDE.md guidance** plus the new
standalone skills — never as edits to `/start` or `/done`.

---

## Tooling facts that drove the design (so we don't relitigate)

**`@kitlangton/stack@0.2.0`** (third-party, MIT, `dist/cli.js`; fork to modify):
- State `.git/stack/state.json` resolves via `git rev-parse --git-common-dir` →
  **shared across all worktrees** of the repo. Safe one-sync-at-a-time; racy if
  two worktrees sync concurrently. (`/restack` enforces one-at-a-time.)
- `StackLink { branch, parent, anchor, pr, headRepository? }`; flat array, one
  parent per branch (linear chain). Multiple independent stacks allowed (parallel
  lanes = independent trunk-rooted stacks).
- `stack track <branch> --onto <parent>` = pure metadata (merge-base anchor + PR
  number). This is how we seed explicit intent and **bypass its PR-base inference**.
- `stack sync --apply` infers links from PR base branches (we avoid surprises by
  pre-tracking), repairs via cherry-pick replay, retargets PR bases, pushes.
- `stack merge --auto --through` lands through the merge queue + repairs descendants.
- **Conflict = clean bail** (no mid-rebase): aborts, leaves a `backup/...` branch,
  writes `undo.json`, exits nonzero with instructions. This is *better* for
  agent-driven resolution than a paused rebase.
- Never creates branches/worktrees (only draft PRs for tracked branches missing one).

**`wt`** (Michael's own, `~/.wt`, fully modifiable):
- State `~/.cache/wt/state.json` (`WtState` in `core/wtstate.ts`). The
  `stacks` manifest map is the SOLE source of stack relationships; the list
  derives membership/order/spine + diff base from it (`core/stack-layout.ts`
  → `tui/hooks/useWorktreeRows.ts`). No per-slug `parent`, no manual stack
  sections, no reflog detection — all gutted.
- Reflog detection (`core/stack.ts`) and the explicit-parent field/`b`-chord
  are gone; a worktree is a stack slice iff its branch matches a manifest slice.
- `createWorktree(branch, {base})` in `core/lifecycle.ts` is the single
  materialization hook (base honored only for new branches; per-slug lock).
- Sibling branches confirmed safe: `eng-5182-01-foo` → distinct slug, distinct
  SST stage (sha256 of slug, `core/stage.ts`), no collisions.
- Linear association is URL-only (`core/linear.ts`, `LINEAR_SLUG_ID_RE`); the id
  is parsed from the slug, so all slices auto-link to the issue. Linear's own
  GitHub integration matches the id in the branch name the same way.

---

## Status

- [x] CLAUDE.md `## PR Size & Stacking` guidance
- [x] `/split` skill (+ context script)
- [x] `/restack` skill (+ context script)
- [x] `/improve-stacking` meta-skill (routes any workflow tweak to the right place)
- [x] wt implementation brief (was `~/.wt/prompt.txt`; consumed + deleted once built)
- [x] This design doc
- [x] wt: gut reflog heuristics → explicit-only (manifest-driven)
- [x] wt: `stacks` manifest in wtstate (+ strict `validateStackManifest` ingest)
- [x] wt: `wt stack apply` (+ `--from`) / `plan --from` / `status` / `rebase`
- [x] wt: `wt size` (canonical "production line" definition)
- [x] wt: manifest-driven list rendering (implicit sections, tree spine)
- [x] Built via `rulesync` → skills live for Claude Code, Codex, OpenCode
- [x] Dogfood: `/split` on `eng-5182` → 6 chained draft PRs (#4818–#4823), clean
      1/2/3/3/3/2 partition, stack tip == holistic. End-to-end works.
- [x] PR bodies: two-part format + `stack-section.{sh,py}`; six `eng-5182` bodies
      written; `/split` step 7 authors them at materialize.
- [x] Dogfood: first `/restack`-after-merge on `eng-5182` (merged s1, cleaned its
      worktree, rebased s2..s6 onto main). Surfaced + fixed three wt bugs (sync
      flag, reconcile-first, worktree parking); stack landed clean. See Session log.
- [x] wt: `wt stack rebase`/`status` resolve the `stackId` from the current branch
      (`findStackIdByBranch`); `parseFailedBranch` no longer mislabels the
      connective "onto" as the failing branch.
- [x] wt: absorbed the `stack` CLI — native squash-safe engine (`restack-engine.ts`
      `NativeRestackEngine.replaySlice`), `@kitlangton/stack` dropped. Replay is
      `git rebase --onto <newParentTip> <baseSha> <branch>` in each slice's own
      worktree (no parking), force-push, retarget PR base. New `StackSlice.baseSha`
      anchor recorded at apply + advanced per replay. Granular `wt stack reconcile`
      / `replay` / `rebase` subcommands. PR-body block + GitLab + merge-queue
      landing intentionally NOT ported. Verified against git fixtures (squash-drop,
      conflict-bail-with-backup, idempotent no-op).
- [x] wt: clean (`c`) auto-reconciles the manifest for any cleaned stack slice
      (`reconcileStack` per affected stack, no replay), plus an
      `effectiveBaseOrTrunk` backstop so a dangling parent base degrades to trunk
      instead of throwing a raw rev-parse error. Replay stays explicit `/restack`.
- [x] wt: `R` keybind — algorithmic fast-path restack. Resolves the selected
      worktree's stack and runs the whole stack through `rebaseStack` (fetch +
      reconcile + squash-safe replay), streaming progress to the activity pane, no
      model input. Clean → done; conflict bail → stops and points at `/restack`.
      Whole-stack (the worktree only selects which stack); already-based slices are
      no-ops.
- [x] wt: stack-on-stack display. `layoutStack` resolves a slice's diff base
      (`parentBranch`) from the manifest `base` verbatim — including an external
      branch (another stack's tip) — decoupled from the in-stack-sibling spine
      classification. A stack-on-stack root now labels + diffs against its real
      parent instead of degrading to trunk. Safe because a dead external ref falls
      back to trunk downstream via `effectiveBaseOrTrunk`.
- [x] wt: `wt stack split <stackId> <sliceId> --from <fragment> [--plan]` — reshape a
      LIVE stack. Replaces one open (or planned) slice with N sub-slices, chains them,
      and re-threads the replaced slice's descendants onto the new tip. Manifest
      bookkeeping only (like `reconcile`); reuses `validateStackManifest` as the safety
      net. New `StackSlice.source` field records the branch the sub-slices reproduce
      their files from (the original slice's branch, since it carries a refactor the
      holistic branch predates); `applyStack` materializes from `slice.source ??
      holisticBranch`. Then `apply` (incremental) + `replay`/`R` (rebase descendants) +
      close the superseded PR/branch. Closes the "edit the middle of the graph" gap.
- [x] wt: **replay stale-anchor self-heal** (eng-5182-04 re-split post-mortem). The
      headline friction across the restack + re-split: `resolveAnchor` trusted the stored
      `StackSlice.baseSha` unconditionally, so after a conflict bail + manual `git rebase
      --onto` + force-push (which never updates the manifest), the next `replay` cut from
      the stale anchor and re-applied the parent's already-present commits → a bogus
      conflict on an already-correct slice. Now `baseSha` is trusted only while it's still
      an ancestor of the branch (`merge-base --is-ancestor`); when the branch was rebased
      off it, replay falls back to the live merge-base with the current parent. Squash
      case unchanged. No new command, no manual bookkeeping. `/restack` step 5 notes
      re-running replay is now safe after a hand-resolve.
- [x] wt: **`wt stack split --apply`** + honest help. `split` is manifest-only; the help
      wrongly implied it ran apply+replay. Fixed the text and added opt-in `--apply` that
      chains reshape → apply → replay (PR retirement stays an explicit printed step).
      `split` also now WARNS which descendant + new-slice PR stack-sections went stale
      (the slice set changed); `/split` regenerates them. Plus silenced the spurious
      "could not tag holistic branch … Failed to resolve" warning on a re-split re-apply
      (only tags when the holistic branch resolves; the archived tag already anchors the
      origin node).
- [x] wt: **replay false "left mid-rebase" fix** (eng-5199 restack post-mortem). The
      engine inferred mid-rebase from `git rebase --abort`'s exit code, which also fails
      when there's nothing to abort — so a PREFLIGHT failure (transient index/ref lock
      from the always-running TUI's concurrent reads, before the rebase started) got
      misreported as a stuck tree on a slice that was actually clean. Now
      `replaySlice` detects a real rebase via the `rebase-merge`/`rebase-apply` state dir
      (`rebaseInProgress`), retries transient preflight failures with backoff, and
      three-way classifies (clean / conflict-with-named-files / preflight-no-start). Plus:
      replay failures persist to the daily app log; conflict errors name the clashing
      files at the CLI; a pass-1 guard refuses to replay into a worktree already wedged
      mid-rebase. Fixture-verified; ultracheck + refute reviewed.
- [x] wt: **cross-stack auto-reconcile**. `reconcileStack` now runs an external-parent
      pass after its own-slice bookkeeping: a live slice whose `base` is neither trunk
      nor a sibling (id or branch) is stacked on another stack's branch — probe that
      branch's PR, and when it's MERGED (or there's no PR and the branch is gone
      everywhere) reparent the slice onto trunk. The `baseSha` anchor keeps the
      subsequent replay squash-safe (the landed parent's commits sit below the anchor
      and are excluded by construction, same as a sibling squash-merge). A still-open
      or merely-closed external parent is left alone. So `/restack` on a stack-on-stack
      child is now fully hands-off across the boundary; fixture-verified both ways
      (merged → reparented, open → untouched).
- [x] wt: **`wt stack add`** — append an EXISTING branch to a live stack as a new tip
      slice (the inverse of `split`'s reshape; the registration path for `wt new
      --base <tip>` + work + "now track it"). Purely additive, so it sidesteps the
      materialized-stack re-ingest guard by construction. Resolves the parent as the
      highest-ordinal live slice (`--onto <sliceId|branch>` overrides; `--onto main`
      roots a new parallel lane), records the squash-safe anchor as
      `merge-base(branch, parent)` (not the parent tip, which may have advanced),
      derives `files` from the anchor diff (doubles as the empty-slice guard), and
      ENSURES a PR — adopts an open one (retargeting its base to match the manifest)
      or pushes + opens a draft. PR-or-create is load-bearing: `validateStackManifest`
      rejects `open` without a `pr`, and a `planned` slice would later be
      re-materialized by `applyStack` from the HOLISTIC branch, clobbering the
      externally-authored content. Never creates branches/worktrees (`wt new` owns
      that). Fixture-verified: adopt + retarget, push + create, anchors, error paths.
- [x] wt: **replay robustness pass** (eng-5183 restack post-mortem, 2026-06-09).
      Four fixes from a real restack that needed three replay re-runs + manual
      babysitting: (1) transient-lock handling got real backoff (5 attempts,
      250ms-linear + jitter — immediate retries kept losing to the same
      gitstatusd lock holder) and now also covers the MID-PICK case: a lock that
      breaks a pick (rebase in progress, zero unmerged paths, lock-shaped
      stderr) is aborted and the whole rebase re-run, and `git rebase --abort`
      itself retries with backoff before ever declaring a worktree stuck.
      (2) The replay gate only blocks on TRACKED changes
      (`worktreeHasTrackedChanges`) — untracked files like the conventionally
      dropped `prompt.txt` ride through a rebase safely and no longer force a
      stash dance. (3) A slice that positionally needs no replay but whose
      remote lags the local tip (the forgot-to-push-after-hand-resolve case)
      gets force-with-lease pushed + PR-retargeted (`pushed` on
      `ReplayOutcome`). (4) Backups self-clean: a clean replay prunes that
      branch's older `backup/restack-*`/`backup/stack-sync-*` refs, and
      `wt stack prune-backups [--days N]` sweeps the rest. Fixture-verified
      end-to-end including a live lock race.
- [x] wt: **replay skips planned slices** (eng-5183 restack friction,
      2026-06-09). `replayStackLocked` now filters `live` to `status ===
      "open"`: a planned slice has no PR and isn't materialized, so any
      branch/worktree already under it is hand-authored WIP the engine must
      neither rebase nor gate on. Previously a dirty planned TIP slice
      hard-failed the whole stack's replay ("uncommitted changes — commit or
      stash") even though the open slices below it never touch that worktree.
      Skipped slices are logged (`skip s4 (…) — planned slice, not yet
      materialized`); they catch up at `wt stack apply` / `wt stack add`. An
      open slice whose PARENT is planned now gets an explicit hint on
      anchor/new-base failure (`parent s4a is still planned; materialize it
      with \`wt stack apply\` first`) instead of the old misleading
      "has no worktree" error, and the dirty-gate message names the slice id.
      `/restack` preconditions updated to "open slices only".
- [x] skill: **non-linear PR-body stack sections**. `stack-section.py` now builds
      the slice tree from `base`/`dependsOn` (already in `wt stack status --json`;
      no wt change). A linear stack renders the flat ordinal-numbered list exactly
      as before; a fork or multi-lane stack renders a nested bullet tree
      (nesting = stacks on, siblings = parallel, one-line legend) with bare
      `#refs` so GitHub keeps expanding live status. No stack-on-stack
      annotation (a `*(stacked on #N)*` note existed briefly; Michael cut it
      2026-06-10 — the external parent stays unmentioned). Cycle-safe
      (malformed bases fall back to the flat list). The section opens with a
      blank line before its `---` so a flush join with the prose can't turn
      the last paragraph into a setext H2.
- [x] wt: **recorded fork base** (`wt new --base` is no longer amnesiac). A
      non-trunk `--base` is persisted per-slug (`baseBranch` + fork-point
      `baseSha` in wtstate), so the TUI's base row, sync counts, diff, and AI
      summary all run against the real parent ("(forked)" suffix) instead of
      main. Deliberately NOT a stack: a fresh fork can't be a valid slice
      (no files/PR; `planned` would be re-materialized from the holistic
      branch), so the hint stays lightweight and the manifest remains the only
      engine input — a manifest slice ignores any vestigial fork record.
      `wt stack add` defaults `--onto` from the record when it names a live
      slice and clears it on promotion; `wt base <slug>` / `set` / `clear`
      inspect + backfill. `/split`'s context script reads it too: a confirmed
      record (HEAD on top of it) resolves the base-detection question as
      case (b) without asking — and it outranks "HEAD is on top of
      origin/main", which is also true whenever the parent chain was just
      rebased onto main.
- [x] wt: `wt stack apply --verify` (opt-in, 2026-06-16). Before creating any
      branch/PR, reconstruct each slice's MATERIALIZED tree (base + its
      ancestor-closure, via `transitiveAncestors` — NOT a monotonic prefix, which
      would leak a parallel lane's content) into a throwaway detached worktree at
      the holistic base, symlink the holistic worktree's deps in (`[stack]
      verify_deps`, default `node_modules`), and run `[stack] verify_command`
      against each, aborting on the first red. Slices stay install-free; the deps
      live in the throwaway worktree. New `core/stack-verify.ts`. Documented
      limitations: wholesale dep symlink can mask a missing-import failure, the
      verify command/tsconfig are read per-prefix, single-root projects only, and
      single-source stacks only (re-split mixed-source bails to CI). Default off.
- [x] wt: **hunk-level slice partitions** (eng-5229 friction, 2026-06-16). Relaxed
      the atomic-file rule: a `StackSlice` may now carry `partials: [{ file,
      hunks }]` so a single changed file's hunks can span slices. New
      `core/hunks.ts` parses the holistic diff into content-hashed (line-shift
      stable) hunk ids, reconstructs a slice's exact intermediate content (base +
      owned-by-self-and-ancestors hunks) as pure text replay, and counts owned
      lines for sizing. `materializeSliceCommit` writes reconstructed partials
      instead of a whole-file checkout — no `git apply`, so materialize never
      conflicts; replay's conflict → `/restack` bail is unchanged. `applyStack`
      gates on a coverage check (every holistic hunk owned exactly once, no stale
      ids, not binary) before touching git. `validateStackManifest` adds the
      structural net (file not both whole-and-hunk, no hunk owned twice, slice
      must own something). `wt stack hunks [--holistic <b>] [--json] <file>...`
      lists canonical ids so `/split` assigns without re-implementing the hash.
      Whole-file slices unchanged; `partials` absent on every existing manifest.
      Verified with git fixtures (reconstruct chain, new-file split, no-eol,
      order-independence) + validation unit checks. **Skill side still TODO:**
      `/split` must learn to detect separable hunks and author `partials`.

---

## Open questions / what might need tweaking

Track friction here as the workflow gets used. Candidate adjustments:

- **Budget numbers** (≤3 files / ~150 prod LOC). If the vibe still fires, go
  smaller. These are guesses; tune against real reactions.
- **File-level vs hunk-level.** RESOLVED (2026-06-16): hunk-level is now
  supported alongside file-level (which stays the default). A slice may own
  whole files (`files`) AND/OR partial files by hunk (`partials: [{ file,
  hunks }]`). The atomic-file rule — one changed file to one slice — was
  blocking legitimately-clean stacks where a single file's hunks served
  different slices (eng-5229: `createMessage.ts` had an actor hunk and a gate
  hunk; three `*.spec.ts` had a fixture-stub hunk and a behavior hunk). Hunk
  ids are content-hashed against the holistic diff (`core/hunks.ts`), so they
  survive line-number shifts; `/split` lists canonical ids via `wt stack
  hunks`. Materialize reconstructs the exact intermediate content (base + the
  owned-by-this-slice-and-its-ancestors hunks) as pure text replay — no `git
  apply`, no fuzz, so **materialize never conflicts**. The fragility the
  file-level default avoided lives only in *replay* (rebasing an
  already-authored partial slice onto a moved parent), which keeps its
  existing conflict → bail → `/restack` path unchanged (Michael's chosen
  semantics). Caveat surfaced while building: two edits within ~3 lines of
  each other coalesce into ONE git hunk and can't be separated at the default
  context — `/split` treats that as an indivisible unit, OR drops the diff
  context via `wt stack hunks --unified 0` and pins `hunkContext: 0` on the
  manifest so the two split apart (the level is pinned per-stack because
  content-hashed ids depend on it). Whole-file slices are byte-for-byte
  unchanged; `partials` is optional and absent on every existing manifest.
  Follow-up (2026-06-16): the compiling-at-every-slice property a whole-file
  stack got for free is now restorable on hunk stacks via `apply --verify`
  (see Status) — it reconstructs each slice's tree and typechecks it before
  any PR opens.
- **Engine: keep `stack` or internalize into wt?** RESOLVED (2026-06-08): internalized.
  Native `NativeRestackEngine.replaySlice` replaced `@kitlangton/stack`. Scoped to
  this workflow only — GitHub, squash-merge, worktree-per-slice. The squash-safe
  guarantee comes from the `StackSlice.baseSha` anchor, not a separate engine state.
  The `RestackEngine` seam stays as the internal boundary. NOT ported (deliberately):
  the PR-body block, GitLab, merge-queue landing (you land via GitHub / the `m`
  keybind), `doctor`/`diagram`, English-output parsing.
- **PR-body `<!-- stack:links -->` block.** RESOLVED by the absorption: the native
  engine doesn't write a nav block at all, so `/split` stays the sole author of PR
  bodies. (The old `stack sync` rewrote every body between `<!-- stack:links:start
  -->`…`<!-- stack:links:end -->`, which Michael disliked and there was no flag to
  disable.) Refinement (2026-06-08, re-split post-mortem): the "section never needs
  updating, GitHub renders status live" property holds only while the slice *set* is
  fixed. A mid-stack re-split changes the set (adds sub-slice PRs, supersedes one), so
  descendant + new-slice sections go stale. wt won't author bodies, so `split` *flags*
  the stale ones and `/split` regenerates them — the seam stays "wt detects, skill
  writes," consistent with the locked decision.
- **Stack-on-stack / polymorphic `base`.** DECIDED (2026-06-08): keep `base` a
  **string** — trunk name, sibling slice id, or an external branch (another stack's
  tip, or an unmerged parent PR). A "stack on a stack" is just a slice whose base is
  the other stack's tip branch; the `baseSha` anchor makes that squash-safe for free
  (the exact footgun that needs squash-safe handling). Do NOT build a super-stack
  container or a structured `{stack, slice}` base now — it's modeling weight for a
  rare trigger. Replay already resolves an external-branch base + records its anchor.
  Validated on a real case (2026-06-08): eng-5183 was forked off the eng-5182 stack
  tip; `/split` correctly recorded `eng-5183-s1.base =
  michael/eng-5182-06-register-builder-tools`. Surfaced a *display* gap — `layoutStack`
  only emitted a diff base for in-stack sibling parents, so the stack-on-stack root
  showed `main` and diffed fat. FIXED: `parentBranch` now follows the manifest `base`
  verbatim (external branches included), decoupled from spine classification.
  The remaining piece landed 2026-06-09: cross-stack **auto-reconcile** —
  `reconcileStack`'s external-parent pass detects the parent merged/deleted and
  reparents onto trunk (see Status), so cross-boundary landing is now the same
  hands-off `/restack` as everything else.
- **PR body authoring split.** RESOLVED: `/split` writes bodies at materialize —
  intent prose (CLAUDE.md rules) + a generate-once stack section
  (`stack-section.sh`). Not `/done`. See Locked decisions.
- **`runInstall` per slice.** RESOLVED: default OFF, slice worktrees are
  install-free (slice == light worktree); install on demand only when you go edit
  a slice (e.g. a `/restack` conflict).
- **Draft-PR noise.** Opening N draft PRs at split time is visible to the team.
  If that itself draws reaction, consider opening lazily per-slice-when-ready.
- **Verify-build cost in `/split`.** RESOLVED: opt-in `wt stack apply --verify`
  (typecheck prefixes in the dep-having holistic worktree); default leans on
  per-slice CI.
- **Conflict-resolution faithfulness in `/restack`.** Must verify with typecheck
  and consult the holistic tag / original convo; never resolve blindly to pass.
- **How `/done` detects a manifest.** The lookup primitive now exists:
  `findStackIdByBranch(branch)` in `core/wtstate.ts` (also powers `wt stack
  rebase`'s id-from-cwd resolution). `/done` still needs wiring to call it and
  ship the whole stack when it hits.
- **Ordinal vs semantic naming** for parallel lanes — confirm Linear + wt display
  both read cleanly in practice.

---

## Session log

- **2026-06-16** — Relaxed the **atomic-file rule** to allow **hunk-level slice
  partitions** (via `/improve-stacking`, prompted by eng-5229 where one file's
  actor/gate hunks and three specs' fixture/behavior hunks couldn't be separated,
  collapsing an otherwise-clean 4-slice stack into one indivisible blob). wt-side
  only, all in `~/.wt`: new `core/hunks.ts` (content-hashed hunk ids,
  base+owned-subset reconstruction as pure text replay, line counting);
  `StackSlice.partials` schema + lenient parse + strict `validateStackManifest`
  net; `materializeSliceCommit` reconstructs partials (never conflicts — replay
  keeps its `/restack` bail); `applyStack` coverage gate (every holistic hunk
  owned once); `wt stack hunks` CLI for `/split`; `splitStack`/fragment parser
  thread `partials`. Verified via git fixtures + validation unit checks;
  typecheck clean. Scope decisions (Michael): only the atomic-file rule (not the
  size budget or indivisible-unit guidance); replay bails to `/restack` on
  conflict (no 3-way). Then `/ultracheck` (12-agent swarm) caught a real
  correctness hole pre-merge: `transitiveAncestors` walked `dependsOn` only,
  but `topoSortSlices`/`resolveParentBranch` also treat `base`-as-sibling-id
  as a parent edge — so a slice parented via `base` alone would reconstruct
  partials without its parent's hunks, silently regressing the shared file.
  Fixed to use the same effective-edge set. Also added: a chain-linearity gate
  (a partial file's owners must form one dependency chain, else no tip carries
  the whole file), a single-source-per-file gate, a worktree path-traversal
  guard in `validateStackManifest`, a NUL-byte/binary guard, wrapped the
  reconstruction write (+`mkdir -p`) so a failure can't half-push a stack, and
  pinned the diff base across the coverage gate and materialize so they
  provably agree. **Follow-up:** teach `/split` to author `partials` (it still
  slices file-only today) — the engine is ready, the planner isn't.
- **2026-06-16** — Hardened the hunk engine with three additions (asked "is it
  good or should we improve anything"): (1) **`wt stack apply --verify`** (new
  `core/stack-verify.ts`) restores the compiles-at-every-slice property
  hunk-splitting forfeited — reconstructs each slice's materialized tree (base +
  ancestor-closure) into a throwaway worktree at the holistic base with deps
  symlinked from the holistic worktree, runs `[stack] verify_command` per slice,
  aborts on the first red, all before any PR opens. (2) **Configurable diff
  context** — a `context`/`manifest.hunkContext` value (default 3) threaded
  through `fileHunks` so `wt stack hunks --unified 0` + `hunkContext: 0` split
  edits the default context coalesces; pinned per-stack because content-hashed
  ids depend on it. (3) **Golden tests** (`core/hunks.test.ts`, `bun test`)
  pinning `parseFileDiff`/`reconstructFile` against real `git diff` output
  (insert/delete/replace, no-EOL both sides, append-at-EOF, duplicate `~N` ids,
  middle-subset of a 3-hunk file, id stability under line shifts). Then
  `/ultracheck` (14-agent swarm) caught a **critical**: the verify path's first
  cut used a monotonic prefix accumulator, which leaks a parallel lane's content
  into a slice that doesn't descend from it (materialize uses the
  ancestor-closure only). Rewrote verify to reset-to-base + reconstruct the
  ancestor-closure per slice (correct for forests/diamonds, not just chains), and
  hoisted `transitiveAncestors` into `stack-layout.ts` so verify and materialize
  share one definition. Swarm also drove: a stale-hunk-id guard + path-traversal
  guard in verify (match materialize), a `git worktree prune` + random-nonce tmp
  path (no stale-registration wedge), timeout-vs-failure distinction, surfacing
  the failing command's output, filtering `wt-verify-*` worktrees from the TUI
  list, and honest dep-symlink/single-root limitation docs.
- **2026-06-08** — Designed the whole workflow (this doc, CLAUDE.md guidance,
  `/split`, `/restack`, `~/.wt/prompt.txt`). Researched `stack` + `wt` source in
  depth to settle the state-vs-engine split. Locked: advisory budgets,
  explicit-only, file-level, holistic-then-split, wt-owns-state. wt
  implementation not yet built. Born out of rebasing the `eng-5182` branch and a
  conversation about PR-size pushback.
- **2026-06-08** — Added `/improve-stacking` meta-skill: one entry point that
  routes a plain-language friction to the owning component (wt code, a skill,
  CLAUDE.md, or this doc), keeps shared contracts coherent, and updates this doc.
  This is the intended way to evolve the workflow from here.
- **2026-06-08** — Dogfooding `/split` on `eng-5182` exposed a stale-base bug: the
  context script defaulted to a local `origin/main` that was behind (the parent
  5181 had just merged), so the diff folded merged work into the slices. Fix:
  `/split` (and `/restack`) context scripts now `git fetch origin` first, and
  `/split` hard-stops with a "STALE base — rebase first" message when `origin/main`
  isn't an ancestor of HEAD. Lesson baked in: always split against fresh main.
- **2026-06-08** — Correction: skills + CLAUDE.md guidance are GENERATED by
  `rulesync` from `~/.dotfiles/.rulesync/` (source), not `~/.dotfiles/ai/`
  (output). First pass wrongly edited `ai/`; relocated all three skills to
  `.rulesync/skills/` (with `targets: ['*']`) and the guidance to
  `.rulesync/rules/CLAUDE.md`, then ran `scripts/rulesync.sh`. Routing in
  `/improve-stacking` + this doc now point at `.rulesync/` source and require a
  rebuild step. **Rule: never edit `~/.dotfiles/ai/`.**
- **2026-06-08** — Continued the `eng-5182` dogfood; three design calls settled:
  (1) **Slice == worktree stays** — I'd proposed decoupling them (worried 6
  worktrees was heavy); rejected as more confusing, and worktrees are light when
  created install-free. Verification therefore runs in a dep-having checkout, not
  the install-free slice worktrees. (2) **Ingestion boundary fixed in spec**: the
  built `wt stack apply` only took `<stackId>` with no way to load a manifest,
  which would have forced the skill to hand-write `state.json`. Decision: wt adds
  `apply --from <file>` with strict validation; the skill pipes JSON through it
  and never touches wt state. (3) **Base detection softened**: the stale-base
  hard-stop was over-fit (it would mis-advise a branch legitimately stacked on an
  unmerged parent); `/split` now fetches then *asks* (a) rebase-onto-main vs
  (b) base = unmerged parent. Skill + doc updated now; the wt `--from` + strict
  validation change is staged in `/tmp/eng-stacking-wt-ingest-prompt.txt` to merge
  into `~/.wt/prompt.txt` once the wt repo's in-flight edits settle.
- **2026-06-08** — First successful end-to-end run. wt landed `apply --from` +
  `plan --from` with strict validation. `/split` on `eng-5182` (14 files) →
  `plan --from` validated, `apply` materialized 6 chained draft PRs (#4818–#4823)
  off main, each ≤3 files (s4 oversized/flagged), stack tip reproduces the holistic
  diff exactly. Whole flow works. Remaining polish: PR bodies are wt stubs (enrich
  later), per-slice typecheck relies on CI (slice worktrees are install-free), and
  the old single PR #4798 should be closed as superseded.
- **2026-06-08** — Implemented the staged ingest change in wt (`prompt.txt` was
  already deleted, so the detail folded into "Where each piece lives" above, not
  `prompt.txt`). Added `wt stack apply --from <file>` + `wt stack plan --from
  <file>`, a STRICT `validateStackManifest` in `core/wtstate.ts` (distinct from the
  lenient read-path `parseManifest`; errors loud on unknown keys, missing
  id/branch/files/ordinal/base, dangling/self deps, dup id/branch, oversized w/o
  reason). Confirmed NO per-slice build gate (slices stay install-free). Fixed the
  non-main-`base` case end-to-end: split `isTrunkBase` out of `isLaneRoot` so a
  stack rooted on an unmerged parent PR branches off + targets + tracks that parent
  branch instead of silently using `origin/main`.
- **2026-06-08** — PR bodies + format. Decided the two-part body (intent prose +
  generate-once "Stack" section) and that the section is NOT maintained — GitHub
  renders each `#ref`'s live status. Added `split/scripts/stack-section.{sh,py}`
  (reads `wt stack status --json`; gotcha: `wt` is a shell alias, so the script
  resolves `~/.wt/bin/wt`). Wrote all six `eng-5182` bodies (#4818–#4823). Wired
  `/split` step 7 to author bodies at materialize. Improvement 1 lives as opt-in
  `wt stack apply --verify` (still TODO in wt — see Status). Note: `~/.wt/prompt.txt`
  was deleted; the wt contract now lives in this doc, references updated.
- **2026-06-08** — First `/restack`-after-merge run (eng-5182: merged s1 #4818,
  cleaned its worktree, rebased s2..s6 onto main). Exposed three wt bugs, all
  fixed: (1) `restack-engine.sync()` passed `--apply`, but the current `stack sync`
  has no such flag (it mutates by default, `--dry-run` previews); (2) `rebaseStack`
  ran `reconcileMerged` *after* the sync loop, so a merged+deleted lane root broke
  sync with "not part of a tracked stack" — moved reconcile to run FIRST (idempotent
  post-sync call kept for `--merge`); (3) the engine moves descendant branches with
  `git branch -f`, which git refuses for a branch checked out in a worktree, and in
  the wt model every slice IS a worktree — added park/unpark (detach each slice
  worktree's HEAD around the engine run, restore via try/finally, refuse dirty). A
  squash-merged parent's duplicate commit is NOT a hand-resolved conflict: once
  parking let the engine move branches, its squash-safe replay dropped every
  descendant's duplicate automatically (the manual `git rebase --onto` mid-run was
  only needed because the three bugs masked each other). Follow-ups landed here:
  `wt stack rebase`/`status` resolve the stackId from the current branch
  (`findStackIdByBranch`), and `parseFailedBranch` stopped mislabeling the
  connective "onto" as the failing branch. /restack skill updated. Pinned for a
  later decision: the engine's unconfigurable `<!-- stack:links -->` PR-body block
  (see Open questions). DECIDED this run: absorb the `stack` CLI into wt (drop the
  `@kitlangton/stack` dep) — design discussion + implementation still to come.
- **2026-06-08** — Absorbed the `stack` CLI into wt (the global binary + personal
  `stack` skill were already removed, so `wt stack rebase` was broken until this).
  `restack-engine.ts` is now a native `NativeRestackEngine.replaySlice`: per slice,
  `git rebase --onto <newParentTip> <baseSha> <branch>` in that slice's OWN worktree
  (HEAD rebases in place → no `git branch -f`, so the park/unpark added earlier this
  day is gone), force-with-lease push, leave a `backup/...` branch only on a conflict
  bail. The squash-safe guarantee is the new `StackSlice.baseSha` anchor (the parent
  tip a slice's commits sit on), recorded at `applyStack` and advanced after each
  replay; replay `--onto`s from it, so a squash-merged parent's duplicate is excluded
  by construction (no patch-id guessing). `stack-ops.ts` split into granular
  `reconcileStack` (manifest bookkeeping only) + `replayStack` (squash-safe replay +
  `gh pr edit --base` retarget, flock-serialized) + thin `rebaseStack` (reconcile
  then replay); CLI gained `wt stack reconcile` / `replay`. Per Michael's steer:
  scoped to his GitHub/squash-merge/worktree-per-slice flow, no `doctor`-style extras,
  granular subcommands so the skills drive complex cases with CC's judgment. NOT
  ported: the PR-body links block, GitLab, merge-queue landing. Decided alongside
  (see Open questions): keep `base` a string so a "stack on a stack" is just an
  external-branch base made squash-safe by the same anchor — no super-stack, no
  structured base; cross-stack auto-reconcile is the sanctioned future extension.
  Verified the replay against git fixtures: squash-dup drop, clean conflict bail with
  backup, idempotent no-op. /restack skill rewritten for the native model.
- **2026-06-08** — Auto-reconcile on clean. After merging slice 01 and pressing `c`
  to clean its worktree, the next slice surfaced a raw `rev-parse`/`unknown revision`
  error in the detail pane: cleaning deletes the parent branch but never told the
  manifest, so the child still recorded `base: <deleted-branch>` and every
  `<base>...HEAD` git call (`syncState`'s `rev-list` via `runOk`, `gitActivity`)
  threw. Root cause is manifest staleness, not git. Two fixes: (1) `doClean`
  (`tui/app.tsx`) now resolves each cleaned branch's stackId
  (`findStackIdByBranch`) and runs `reconcileStack` once per affected stack —
  manifest-only bookkeeping (mark merged, reparent orphans onto trunk), no
  rebase/push, so it's safe off a keystroke; the orphan re-roots onto trunk and the
  error clears. The heavier **replay** (rebasing commits off the squashed parent,
  force-push) deliberately stays an explicit `/restack`. (2) Backstop:
  `effectiveBaseOrTrunk(wtPath, base)` in `core/git.ts` rev-parses the base and
  falls back to `origin/<trunk>` when it doesn't resolve; `syncState`, `gitActivity`,
  and `buildDiffContext` all route through it, so a dangling parent degrades to a
  (fat) trunk diff instead of a thrown error during the window before reconcile
  lands. An external (stack-on-stack) base still resolves, so it's untouched.
  Verified the fallback against a git fixture. /restack skill noted that clean
  pre-reconciles.
- **2026-06-08** — Added the `R` keybind: the algorithmic fast-path restack. It
  resolves the selected worktree's stack (`findStackIdByBranch`) and runs the whole
  stack through `rebaseStack` (fetch → reconcile → squash-safe replay), streaming
  `onLog` to the activity pane — zero model tokens on the clean path. On a conflict
  bail it stops and toasts `conflict on <slice> — run /restack`, leaving the engine's
  backup branch in place; `/restack` stays the escalation path that owns conflict
  judgment. Whole-stack by design: restack is a coherence operation and the worktree
  only selects *which* stack; already-based slices are cheap no-ops (anchor ===
  newBase skip), so there's no need for a partial "from here down" mode. A
  `restackBusyRef` guards UI re-entry (the engine flock is the real lock). Realized
  the fast path already existed as `wt stack rebase` — this just gives it a
  first-class TUI surface so the 90% clean case never touches a model.
- **2026-06-08** — Stack-on-stack display fix, from a real case. eng-5183 was forked
  off the eng-5182 stack tip; `/split` correctly recorded `eng-5183-s1.base =
  michael/eng-5182-06-register-builder-tools` (an external-branch base, the polymorphic
  `base` string working as designed). But the detail pane showed its base as `main` and
  it diffed fat against trunk. Root cause was a *display* gap in `layoutStack`:
  `parentBranch` (the diff base + base label) was only set from an **in-stack sibling
  slice**, and `isLaneRoot` (empty `dependsOn`) classified the external-base root as a
  trunk lane root, so `parentBranch` went null and it fell to trunk. Fix: compute
  `parentBranch` from `resolveParentBranch(manifest, s)` (which passes an external base
  through verbatim) for any non-trunk base, decoupled from the in-stack-sibling spine
  classification. An external-branch root still roots its own *section* (its real parent
  lives in another stack's section and can't be drawn under it), but now labels + diffs
  against that real parent. Safe because `effectiveBaseOrTrunk` degrades a dead external
  ref to trunk at the git layer (the just-shipped backstop). Verified `layoutStack`
  against the live manifest: eng-5183 s1 now resolves `parentBranch =
  michael/eng-5182-06...`. Promoted cross-stack **auto-reconcile** (reparent a child
  stack onto trunk when its external parent merges) from a future-extension musing to a
  tracked Status TODO, since the pattern is now real.
- **2026-06-08** — `wt stack split`: reshape a live stack. Hit on eng-5182-04
  (save-metric, 629 prod lines / 5 files, ~4× budget) — an already-open mid-stack
  slice that needs re-splitting, which no command supported: `apply` only materializes
  a fresh manifest, `reconcile`/`replay`/`rebase` preserve the existing shape. Added a
  shape-changing primitive `wt stack split <stackId> <sliceId> --from <fragment>
  [--plan]` that replaces an open slice with N sub-slices (chained), re-threads the
  replaced slice's children onto the new tip, and removes the replaced slice. Pure
  manifest bookkeeping (no git/PRs) like `reconcile`; runs the reshaped manifest
  through the strict `validateStackManifest` before writing. The sub-slices partition
  the ORIGINAL slice's branch (not the holistic branch — it can carry a refactor the
  holistic predates), recorded via a new `StackSlice.source` field that `applyStack`
  reproduces from (`slice.source ?? manifest.holisticBranch`). Flow: refactor + commit
  on the slice branch → `split --plan` preview → `split` → `apply` (incremental, only
  the new planned sub-slices materialize) → `replay`/`R` (rebase + retarget the
  re-threaded descendants) → close the superseded PR + delete its branch. Key bug
  caught in fixture: a child's `base` is stored as the parent's BRANCH name while
  `dependsOn` uses slice ids, so the re-thread matches both forms and normalizes to the
  sub-slice id. Verified the reshape against the live eng-5182 manifest with `--plan`
  (s4 → s4a..s4d, s5 re-threaded onto s4d, ordinals renumbered, nothing written) plus
  the merged-slice / <2-sub-slice / unknown-slice guards. `/split` skill gained a
  "Re-splitting a live slice (mid-stack)" section. Insert/drop a mid-stack slice are
  the natural siblings, not built yet.
- **2026-06-08** — Post-mortem on the eng-5182-04 re-split (it worked, but with
  friction; reviewed the dogfood session via `/history`). Four fixes:
  (1) **Replay stale-anchor self-heal** — the headline bug, which bit the eng-5182
  restack AND the re-split. `resolveAnchor` trusted the stored `baseSha`
  unconditionally; after a conflict bail + manual `git rebase --onto` + force-push
  (which never updates the manifest), the next `replay` cut from the stale anchor and
  re-applied the parent's already-present commits → a bogus conflict on an
  already-correct slice. Fix: trust `baseSha` only while it's still an ancestor of the
  branch (`git merge-base --is-ancestor`); when stale, fall back to the live merge-base
  with the current parent (post-rebase, exactly the tip the slice now sits on). The
  healthy squash case is unchanged (the unrewritten child still descends from `baseSha`,
  so the squash-merged parent's commits stay excluded). Self-healing, no new command, no
  manual bookkeeping. (2) **`wt stack split --apply`** — the `--help` implied split did
  "apply + replay" but it only reshaped the manifest and printed next steps. Fixed the
  text AND added an opt-in `--apply` that chains reshape → apply → replay; PR retirement
  stays an explicit printed step (it closes a PR + deletes a branch). (3) **Stale
  PR-section flag** — a re-split changes the slice *set*, so re-threaded descendants'
  (and the new sub-slices') stack sections list the superseded PR and omit the new ones.
  wt doesn't author PR bodies (locked decision), so it now WARNS with the exact list;
  `/split`'s re-split section gained a "regenerate the stale stack sections" step. (4)
  **Silenced a spurious apply warning** — re-apply (split → apply) tried to re-tag a
  holistic branch already archived to its tag + deleted, emitting "could not tag …
  Failed to resolve"; now it only tags when the branch resolves and stays quiet when the
  archived tag already anchors the origin node. `/restack` step 5 updated to note replay
  self-heals after a manual resolve.
- **2026-06-08** — Replay engine: fixed false "left mid-rebase" bails (surfaced by an
  eng-5199 `/restack` that landed correct but bailed twice on clean single-commit slices;
  the same `git rebase --onto` succeeded by hand). Root cause: the engine inferred
  "worktree is mid-rebase" from the exit code of `git rebase --abort`, which ALSO returns
  non-zero when there's nothing to abort — i.e. when the rebase failed at PREFLIGHT
  (before it ever started, e.g. a momentary index/ref lock from the always-running TUI's
  concurrent per-worktree git reads). The tree was untouched but the engine screamed
  "left mid-rebase." Fix in `NativeRestackEngine.replaySlice`: detect a real in-progress
  rebase authoritatively via the `rebase-merge`/`rebase-apply` state dir (`rebaseInProgress`),
  not the abort exit code; retry a transient preflight failure a few times with backoff
  (the manual retry worked because the lock had cleared); and three-way classify — clean
  success / genuine conflict (abort, `conflict:true`, backup kept, now names the conflicting
  files via `--diff-filter=U`) / preflight no-start (clean retryable error with git's
  stderr). Diagnosability: replay failures now persist to the daily app log (`log.warn`
  with anchor/newBase/backupBranch) since the engine only streamed to the console before,
  and an eng-5199-style CLI run left nothing to inspect. Also added a pass-1 guard that
  refuses to replay into a worktree already wedged mid-rebase from an interrupted run (the
  porcelain dirty-check can read clean for such a tree, and replaying would silently abort
  its in-flight state). Verified with a git fixture (clean replay + genuine conflict:
  correct classification, clean tree, no rebase left in progress, named conflict file).
  Reviewed via an ultracheck swarm + 3-lens refute.
- **2026-06-09** — Appending to a live stack + landing across stack boundaries, both
  surfaced by the eng-5182/eng-5183 stack-on-stack work. (1) **`wt stack add`** (new
  subcommand, `addSliceToStack` in `stack-ops.ts`): registers an EXISTING branch as a
  new tip slice on a materialized stack — the gap found at the end of the
  eng-5182-04b session, where the only options were a full re-ingest (refused on
  materialized stacks, by design) or hand-editing `state.json`. Additive-only, so the
  re-ingest guard doesn't apply. Default parent = highest live slice; `--onto`
  overrides (slice id, slice branch, or trunk for a new lane); anchor =
  `merge-base(branch, parent)`; files = anchor diff; PR adopted (base retargeted to
  match the manifest) or draft-created — required, because `planned` would get
  re-materialized from the holistic branch by a later `apply` and `open` without a
  `pr` fails validation. `viewPrInfo` gained `title` so an adopted PR's title becomes
  the slice title. (2) **Cross-stack auto-reconcile** (closes the Status TODO):
  `reconcileStack` gained an unconditional external-parent pass — a slice based on a
  non-trunk, non-sibling branch whose PR is MERGED (or branch gone with no PR) is
  reparented onto trunk; open/closed parents are untouched. Replay stays squash-safe
  via the existing anchor. The own-merge early-return became a guarded block so the
  external pass always runs. Both verified with a bare-origin git fixture + fake `gh`
  shim (20 asserts: adopt/retarget/create/anchors/error paths; merged-external
  reparented, live-external untouched).
- **2026-06-09** — Replay robustness pass, from the eng-5183-01 `/restack`
  post-mortem (the run worked but needed three replay re-runs, a manual
  `rebase --abort`, a stash dance, and a hand push). Four wt fixes:
  (1) **Lock races**: retry backoff is now real (5 attempts, 250ms-linear +
  jitter — the old quick retries kept losing to the same gitstatusd lock
  holder) and extends to the two previously fatal shapes: a lock that breaks a
  pick MID-rebase (in-progress + zero unmerged paths + lock-shaped stderr →
  abort + re-run the whole rebase, distinguished from a genuine conflict by
  `--diff-filter=U`), and `git rebase --abort` itself losing to the same lock
  (`abortRebaseWithRetry`, success judged by `rebaseInProgress`, not exit
  code). (2) **Untracked files don't block replay**: the gate switched from
  `worktreeIsDirty` to a new `worktreeHasTrackedChanges` (porcelain with
  `--untracked-files=no`) — `git rebase` is safe alongside untracked files and
  the workflow itself drops `prompt.txt` into slice worktrees, so blocking on
  them was self-inflicted. `wt rm`'s lose-work warning keeps the strict check.
  (3) **Skip-path push backstop**: a slice already on its base whose remote
  lags the local tip (hand-resolved conflict, forgotten push) is now
  force-with-lease pushed and PR-retargeted; `ReplayOutcome` carries `pushed`
  alongside `moved`. A planned slice with no origin ref is left alone.
  (4) **Backup hygiene**: a clean replay prunes that branch's older
  `backup/restack-*` + legacy `backup/stack-sync-*` refs (they're superseded;
  commits stay in the reflog), and `wt stack prune-backups [--days N]` sweeps
  the rest (default 0 = all). `/restack` skill updated: tracked-only
  precondition, lock-retry + prune-backups notes, manual push marked as
  backstopped. Fixture-verified end-to-end, including a live `index.lock` race
  (lock dropped mid-run at +600ms; attempt 2 succeeded).
- **2026-06-09** — Replay skips planned slices. Friction from the eng-5183
  restack: `wt stack rebase eng-5183` hard-failed because the PLANNED tip slice
  (eng-5200-preserve-user-cells, no PR) had live WIP in its worktree — the
  dirty gate covered every non-merged slice, so untouchable in-progress work on
  a slice the replay would never need blocked rebasing the three open slices
  below it. (Bonus confusion: the slice's branch carries a different issue id,
  so the error read like a foreign stack.) Fix in `replayStackLocked`
  (`stack-ops.ts`): `live` filters to `status === "open"`; planned slices are
  skipped with an onLog notice and catch up at `apply`/`add`. Rationale: a
  planned slice's branch/worktree (when one exists at all) is hand-authored WIP
  — rebasing it would clobber, gating on it blocks, so the engine ignores it
  entirely. Open-slice-on-planned-parent failures (anchor/new-base unresolvable
  because the parent branch doesn't exist yet — the normal split-before-apply
  state) now carry a `plannedParentHint` pointing at `wt stack apply`, replacing
  the old "has no worktree" misdirection. Dirty-gate error now names the slice
  id. `/restack` skill preconditions updated (open slices only; planned never
  blocks). Typecheck + smoke clean; no fixture run (gate-scope change only).
- **2026-06-09** — Non-linear stack sections in PR bodies. The manifest already
  expresses a forest/tree (`base` = trunk | sibling id | external branch; a
  shared base = parallel siblings; joins are inexpressible since `base` is a
  single string — nothing to render there), and `wt stack status --json`
  already emits `base`/`dependsOn`, but `stack-section.py` flattened everything
  into an ordinal-sorted numbered list, misrepresenting parallel lanes as a
  sequence. Now the script builds the tree and picks a renderer: LINEAR (single
  root, no fork) keeps the flat numbered list byte-for-byte — zero churn on the
  common case; NON-LINEAR renders a nested bullet tree (nesting = stacks on,
  siblings = parallel, short italic legend) still using bare `#refs` so
  GitHub's live title+status expansion — the load-bearing "generate once,
  never maintain" property — survives. Rejected mermaid/ASCII-tree for exactly
  that reason (code blocks don't expand refs). A stack-on-stack ROOT gets a
  one-time `*(stacked on #N)*` note: the parent PR isn't in the manifest, so
  the script resolves the external base branch via `gh pr view` at generation
  (best-effort; falls back to the backticked branch name; trunk-shaped bases
  skipped). Skill-only change (`split/scripts/stack-section.py` + a step-7
  sentence in SKILL.md); no wt code touched. Verified against synthetic
  status JSON: linear unchanged, fork/lane/planned-slice/external-root all
  render correctly.
- **2026-06-10** — Recorded fork base for `wt new --base`. Real friction: a
  worktree forked off a stack tip (`wt new … --base michael/eng-5201-…`)
  showed base "main" in the details pane and diffed fat against trunk — the
  flag created the branch off the ref and then forgot it. Considered and
  REJECTED the maximalist fix (make `--base` stack-only and auto-create a
  single-slice manifest): a zero-commit fork can't satisfy the slice
  invariants (non-empty `files`, `open` needs a PR, `planned` gets
  re-materialized from the holistic branch and would clobber hand work), a
  standalone parent would force inventing a fake `holisticBranch`, and
  `wt stack add` was already designed as the deliberate, post-work
  registration path. Built instead: per-slug `baseBranch`+`baseSha` in
  wtstate (recorded at create, fork point captured for free as a future
  squash-safe anchor), `resolveStackedOn` falls back to it for non-slice
  worktrees (`via: "fork"`, manifest always wins), base row shows
  "(forked)", `wt stack add` defaults `--onto` from the record and clears
  it on promotion, and a `wt base` show/set/clear subcommand for backfill.
  Lifecycle: forked → (work, PR) → `wt stack add` → tracked slice. A dead
  recorded base degrades to trunk via the existing `effectiveBaseOrTrunk`
  backstop. Mutators preserving unknown slug fields verified (placeSlug
  spread bug fixed); end-to-end fixture-tested create/record/preserve/rm.
- **2026-06-10** — `/split` reads the recorded fork base. The context script
  resolves the diff base itself now: it asks `wt base <slug>` and, when the
  record is CONFIRMED (`merge-base --is-ancestor <rec> HEAD`), declares case
  (b) with that parent and diffs against it — no user prompt. Testing on the
  real eng-5201 worktree exposed why the check must run BEFORE the
  is-ancestor-of-main gate, not just inside its failure branch: the eng-5183
  parent chain had just been rebased onto main, so HEAD was on top of
  origin/main too and the old "base status: OK" path would have folded the
  whole unmerged parent stack into the slices. Order is now: confirmed
  record wins → else on-main OK → else the (a)/(b) ask. A drifted record
  (HEAD not on top of it) is surfaced but never trusted. Skill precondition
  text updated to match; no wt code change.
- **2026-06-10** — Setext-heading bug in PR bodies + annotation cut. The
  eng-5201 bodies rendered their last prose paragraph as a giant bold H2:
  markdown parses `text\n---` as a setext heading, and the assembled body had
  no blank line between the prose and the stack section's `---`. Fixed in
  both layers: `stack-section.py` now emits a LEADING blank line (flush
  joins are safe by construction) and `/split` step 7 spells out the
  assembly as `prose + "\n\n" + section` with the why. Also removed the
  `*(stacked on …)*` root annotation from the tree renderer same-day after
  Michael saw it live on eng-5201 and didn't like it (it also leaked the
  parent branch name; the design already says the PR body never mentions
  the holistic/parent plumbing). All seven eng-5201 bodies regenerated
  (prose kept, section swapped); eng-5184/5185 were assembled with the
  blank line and main-rooted, so untouched.
