---
name: babysit
description: >-
  Drives this worktree's own PR through review-bot rounds and merges it, when
  the human hands you the merge (/babysit, or asks you to land your branch).
  Not for the manager session; fleet-wide work is `shepherd`.
targets:
  - '*'
user_invocable: true
---

# Babysit

**Invoking this hands you the merge for this one branch.** The human
otherwise merges. Merge when the branch has earned it, and say plainly when it
has not; parking at `ready` to ask is the wrong answer.

**Scope: your worktree only.** Never touch another worktree's branch, PR or
threads.

## First: check `WT_AGENT`

Before any command that writes, including `wt status`:

- a slug: that is your row; continue.
- `manager`: stop here. This is the wrong skill: fleet sweeps are `shepherd`,
  and code changes and merges belong to the owning worktree's session.
- unset: find your worktree from the checkout path first.

## The loop

Each pass: read your row, do the one thing it calls for, wait minutes (not
seconds), repeat. Re-read every pass; the bot, CI and the base all move.

```bash
wt fleet --json | jq '.[] | select(.slug == env.WT_AGENT)'
```

Read review state from this row, not from `gh` or the bot's comments.
`.pr.review_bot` already separates the bot's full passes from its appended
per-commit delta findings; your own parsing will disagree with the badge the
human sees.

- `.pr` null: no PR when `.pr_note` is null; GitHub unreachable when it is set.
- `.pr.review_bot`: `{state: pending|unresolved|clean|none, unresolved, stale}`.
  `stale: true` means the bot has not reviewed your current head, so `clean`
  there is a pass on old code, not on yours.
- `.pr.checks` (`pass|fail|pending|none`), `.pr.mergeable` (`conflicting`
  is yours to fix), `.pr.merge_state`, `.pr.draft`,
  `.pr.unresolved_threads` (all), `.pr.unresolved_human_threads`.
- `.work.blockedOn`, `.work.verifyAfterMerge`, `.base` (your parent when stacked).

| state | do |
|---|---|
| no PR | finish and push the work first (`start`) |
| draft | mark ready only when finished; see *One-shot review* |
| bot `pending` | wait |
| bot `unresolved` | address the findings |
| `clean` + `stale` | wait for the review of the current head |
| `clean`, not stale, checks not failing, no threads | merge (see *Merging*) |
| `checks: fail` | read the failure first; see *Red review check* |
| `mergeable: conflicting` | rebase onto the base and resolve; `wt restack` if stacked |
| open human threads | reply and resolve them |
| merged, more work asked | *Next PR from this worktree* |

## Findings

Each finding takes four separate actions; none implies another:

1. Fix it, or decide not to.
2. Reply on the review thread itself, saying what you did or why you
   declined. A top-level PR comment is not a thread reply.
3. Resolve the thread. Resolving silently reads as "implemented".
4. Tick its box in the bot's summary comment, if it keeps one. The PR summary
   counts boxes, not threads.

Keep going after the first pass: the review that counts covers the head you
pushed last, and every round of fixes creates a new one.

## Pushing

A review belongs to one head SHA, and some bots have a per-head budget.
Batch: fix the whole round, verify locally, push once. Then update the PR body
and any change guide your instructions require, and re-run `wt guide <url>`
for a recorded guide. To re-run a check, re-run the check; never push (or
push an empty commit) to re-trigger anything.

## Red review check

| shape | meaning |
|---|---|
| `skipped`, 0 steps | draft gate |
| `failed`, 0 steps, ~2 s | billing or quota refusal |
| `fail` while still running | interim state |
| `fail` after minutes of real steps | crashed: read the log, then re-trigger the run |

None of these is a finding. Only the last needs action, and it is a
re-trigger, not a push.

## One-shot review

Marking a PR ready triggers a review that will not re-trigger. Do it only once
the branch has stopped moving: work done, checks not failing, own review done.

## Merging

Merge when, on the current head: the bot is `clean` and not stale, checks
are not failing, no open threads, not `conflicting`, and none of these holds:

- `blockedOn` is set. `wt status --unblock` only if the gate is satisfied;
  otherwise say why you stopped. Re-asserting `ready` without `--blocked-on`
  clears the gate, so check it first.
- Your stack parent (`.base` is another live worktree's branch) has not
  landed. If the order looks wrong, `wt manager send`.
- You cannot honestly claim `low` or `medium` risk. Verify it, or hand the
  risk back with `needs-human`.
- The human said not to.

Anything else (a declined finding, a re-triggered check, a note for the human)
goes in the note, and you merge. Record what lands, then arm it:

```bash
wt status ready --risk <low|medium> -m "<note>"
wt merge
```

`wt merge` arms GitHub's merge-when-ready, choosing the merge queue or classic
auto-merge from the PR's base branch, so checks still running are fine. Armed
is finished: do not watch the queue or merge again. Use `wt merge`, never
`gh pr merge`: `--auto` fails on queue bases with a misleading "Auto merge is
not allowed" error, and a queue sets its own merge method. Exit 75 means a
required check has not reported yet: wait and re-run, never escalate.
`wt merge --cancel` disarms and dequeues.

## After it lands

- `verifyAfterMerge` set: confirm the deploy carrying the change is live, run
  the steps, then `wt status verified -m "<what you checked, where>"`.
- Do not remove the worktree; merge cleanup archives it.
- Stop when merged with verification done, blocked on a human, or a pass finds
  nothing to do or wait for. Never keep polling a clean current-head review;
  merge it.

## Next PR from this worktree

When asked for more work after the merge, run `wt next` (with
`--issue <ID>|none` or `--gh <N>|none` for a different issue): it moves the
worktree to a fresh branch from the latest base and keeps its sessions,
section and history. Then `wt status working`, commit,
`git push -u origin HEAD`, open the PR and run this loop. Never reset the
merged branch onto the base by hand; the merged PR stays attached and the row
can read as landed.
