---
name: triage
description: >-
  Tiers a pasted batch of tracker tasks, creates worktrees for the approved
  batch, or reviews a batch in flight. Start with /triage <tasks> | review |
  create <tier>.
disable-model-invocation: true
targets:
  - '*'
argument-hint: "<pasted tasks: ids + titles/descriptions> | review | create <which tier(s)>"
user_invocable: true
---
# Triage

Turn tracker tasks into a prioritized batch, then into ready-to-work
worktrees; or report on a batch in flight. This ends where `/start` begins:
you prepare worktrees, `/start` implements inside each.

Tasks usually arrive pasted (ids plus whatever text the user gave). Use only
what was pasted: if a task is too thin to tier, say so and ask for its
description. The human owns tracker status; never claim to have changed it.

## Project conventions

{{project_notes}}

## Picking the mode

- **triage**: pasted tasks. Read-only.
- **materialize**: "create", "go", "materialize". Writes.
- **review**: "review", "status". Read-only.

Explicit args win; otherwise infer from the conversation (already triaged?
worktrees in flight?); ask one question only if still ambiguous.

**Write gate:** inference is enough for read-only modes, never for writes.
Before any `wt new` or GitHub issue, restate the exact batch (tasks, groups,
worktrees, stacking) and get an explicit go.

## Mode: triage

1. **Absorb.** Parse (id, title, description). Check what is already in
   flight: `wt ls` (a slug carrying the id is being worked), and
   `gh pr list --state all --search "<id>"` when a task looks done.
2. **Read the code** each task touches, enough to judge scope and catch
   duplicates, overlaps and already-implemented work. Fan out cheap subagents
   for a large batch; verify yourself any code claim that drives a decision.
3. **Tier by the human's testing burden**: can it ship without the human
   iterating on it by hand?
   - *Easy wins*: UI changes, tightly scoped deterministic logic, config or
     default flips, unit-testable bug fixes.
   - *Defer*: prompt or LLM-behavior changes (need eval iteration), large
     architecture changes, anything needing production deploy-gate care.

   Call out duplicates and overlaps (merge them), dependencies (stack with
   `--base`), clusters that touch the same code (one worktree), and tasks that
   need a design pass first.
4. **Recommend a concrete next batch and stop** for the user's pick.

## Mode: materialize

1. **Group** tasks that share a code area into one reviewable worktree.
2. **Create** each with `wt new <ID> <short title words> --no-open`, never
   raw `git worktree add`. The id leads the slug; a second worktree for the
   same id just needs different words. Add `--base <branch>` to stack on
   another worktree in the batch. A GitHub issue is a secondary link
   (`--gh <n>`, or later `wt issue <slug> --gh <n>`), never part of the branch
   name. Work without a tracker task: `wt new <words>`.
3. **GitHub issues only where they earn their keep:** multi-part groups that
   several sessions or worktrees will coordinate on, and design-worthy tasks.
   Small self-contained fixes need none. Reference each as `GH: #N` in the
   brief and record it with `wt issue <slug> --gh <n>`.
4. **Write `prompt.txt`** in each new worktree's root (never for your own
   session), never committed. If an issue holds the spec, the brief just
   points to it. Otherwise it carries what `/start` would have to
   rediscover: the pasted task text, bundled parts and order, `GH: #N`
   references, code to reuse, "verify the current state first" warnings,
   overlaps and in-flight dependencies. End it with:
   `(You can delete this prompt.txt after reading it.)`
   Then seed the row: `wt status <slug> todo -m "<one-line goal>"`.
5. **Start every worktree** with `wt agent start <slug>`, without asking.
6. **Report** a table (tasks, branch, path), which worktrees are independent
   or stacked, and that the sessions were started.

## Mode: review

- `wt status --all` for lifecycle states, then `wt ls` and `wt doctor` for
  worktree health.
- `gh pr list --state all --json number,title,headRefName,state,isDraft,reviewDecision`,
  filtered to the batch, plus the review bot's findings on open PRs.
- Summarize merged / in review / open / in progress and what is next, and
  list ids that look done so the human can update the tracker.

Never suggest cleanup (`wt clean`, `wt rm`, "clean candidates"); the human
sweeps merged worktrees. Run them only when asked.

## Writing

Use full markdown links for issue and PR references, and no em dashes in
anything humans read.

## User Instructions

$ARGUMENTS
