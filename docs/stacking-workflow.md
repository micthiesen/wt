# Small-stacked-PR workflow — design & rationale

> Living design doc. When something in this workflow needs tweaking, read this
> first for full context, then update it (decisions, what works, what to change).
> Last updated: 2026-06-08.

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
- **Base detection asks, never assumes.** `/split` fetches first (main is always
  current). If HEAD isn't on current `origin/main`, it asks: (a) parent merged /
  main moved → rebase onto main (base = main); or (b) intentionally stacked on an
  unmerged parent → base = that parent branch, slices stack on its PR, `/restack`
  rebases onto main once the parent lands. Never silently rebase or split stale.

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
- [ ] Dogfood: split this very `eng-5182` branch as the first real test

---

## Open questions / what might need tweaking

Track friction here as the workflow gets used. Candidate adjustments:

- **Budget numbers** (≤3 files / ~150 prod LOC). If the vibe still fires, go
  smaller. These are guesses; tune against real reactions.
- **File-level vs hunk-level.** File-level keeps slices compiling and is the
  default. If too many "indivisible" files show up, revisit selective hunk
  splitting — but it's where compiling-correctness gets hard.
- **Engine: keep `stack` or internalize into wt?** Day-1 uses `stack` behind the
  `RestackEngine` seam. Revisit if the shared `.git/stack` state (cross-worktree)
  or the dual-tooling causes real friction. Only the cherry-pick replay is hard
  to port.
- **Dual state reconciliation.** wt manifest (truth) vs stack's projected
  `state.json`. Watch for drift; `wt stack status` must surface it, not paper
  over it.
- **PR body authoring split.** wt creates minimal draft-PR bodies at materialize;
  richer intent-first bodies are a skill's job. Decide if `/split` or `/done`
  enriches, and avoid double-authoring.
- **`runInstall` per slice.** Each slice worktree needs its own node_modules
  (slow). Default off + install where needed, or share. Tune for ergonomics.
- **Draft-PR noise.** Opening N draft PRs at split time is visible to the team.
  If that itself draws reaction, consider opening lazily per-slice-when-ready.
- **Verify-build cost in `/split`.** Typechecking each cumulative prefix is the
  correctness guarantee but is slow; parallelize if it drags.
- **Conflict-resolution faithfulness in `/restack`.** Must verify with typecheck
  and consult the holistic tag / original convo; never resolve blindly to pass.
- **How `/done` detects a manifest.** Needs a reliable "is this branch part of a
  stack?" lookup (by issue id / branch) so the CLAUDE.md "ship the whole stack"
  rule actually fires.
- **Ordinal vs semantic naming** for parallel lanes — confirm Linear + wt display
  both read cleanly in practice.

---

## Session log

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
