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

Each pass: read your row, do the one thing it calls for, wait for the next
change, repeat until the PR has merged. Re-read every pass; the bot, CI and
the base all move.

Stay in the session the whole way. A CI run, a bot round and the merge queue
each resolve in minutes, and a PR whose agent ended its turn is not being
babysat. To wait, block on `wt wait`, never sleep or poll:

```bash
wt wait --any reviewed settled --timeout 30m        # the bot or the checks still running on this head
wt wait --any merged evicted closed --timeout 30m   # armed: until it lands or is dropped
```

- Name only what has not happened yet: a condition that already holds
  returns at once. `settled` is checks passed or failed, `reviewed` is the
  bot finished the current head, `evicted` is the armed merge was dropped.
  Without `--any`, all must hold.
- Prefer a background command (the harness returns you when it exits). Where
  the harness caps foreground commands (Claude Code caps them at 10 minutes),
  pass `--timeout 9m` and run it again on exit 124. It judges the conditions
  itself, so the wt TUI need not be open, and it stores nothing: no watch,
  nothing owed.
- It prints each condition's state. Exit 0: one held, so read the row and
  act. Exit 124: timed out. Read the row after every exit 124, before waiting
  again: a wait that starts over can take two GitHub reads to see an eviction
  the row already shows. Wait again while the thing is still plausibly moving
  (checks running, bot pending, a queue position advancing). Exit 1: it could
  not tell and says why (GitHub unreachable, usually); fix that or wait a few
  minutes in the session.
- Hand the wait to `wt wake <condition> -m "babysit: next pass"` and end the
  turn only when what remains is genuinely long: a queue or CI backed up for
  hours, a bot out of budget until a known time, a human reviewer. A blocked
  command dies with the session and a wake does not, but a wake depends on
  the wt TUI being open and one delivered after the merge is owed, so it is
  the fallback, not the pass.

```bash
wt fleet "$WT_AGENT" --json | jq '.[0]'   # one row; the output is still an array
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
- Armed: `.merge_queue` (null when not queued, else its `state`) and
  `.pr.auto_merge` (classic auto-merge). `.pr.state` is `MERGED` once it lands.

| state | do |
|---|---|
| no PR | finish and push the work first (`start`) |
| draft | mark ready only when finished; see *One-shot review* |
| bot `pending` | wait |
| bot `unresolved` | address the findings |
| `clean` + `stale` | wait for the review of the current head |
| `clean`, not stale, checks not failing, no threads | merge (see *Merging*) |
| armed (queued or `auto_merge`), not merged | wait for it; see *Until it lands* |
| was armed, now neither, or queue `UNMERGEABLE` | evicted: see *Until it lands* |
| `checks: fail` | read the failure first; see *Red review check* |
| `mergeable: conflicting` | rebase onto the base and resolve; `wt restack` if stacked |
| open human threads | reply and resolve them |
| open reader comments on a recorded guide | treat them as human threads: answer or fix, then mark them addressed |
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
  otherwise say why you stopped. A status change keeps the gate: only
  `--unblock`, its `--until` watch, `verified` or `dropped` clears it.
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
auto-merge from the PR's base branch, so checks still running are fine. Use `wt merge`, never
`gh pr merge`: `--auto` fails on queue bases with a misleading "Auto merge is
not allowed" error, and a queue sets its own merge method. Exit 75 means a
required check has not reported yet: wait a few minutes in the session and re-run, never escalate.
`wt merge --cancel` disarms and dequeues.

## Until it lands

Armed is not merged. Queues evict PRs often: a queue run fails, an entry
ahead conflicts, the base moves. GitHub also drops classic auto-merge on a
conflict, and tells nobody.

So stay until it has merged. Set no wake for the landing: a wake delivered
after the merge is an owed step you would then have to close. After
`wt merge`:

1. Run `wt watch` and cancel any pass wake still pending
   (`wt watch cancel <id>`); it would fire after the merge and be owed.
2. Block on `wt wait --any merged evicted closed --timeout 30m`. After a
   timeout (exit 124) read the row, then wait again while the queue or the
   checks are still moving.
3. `merged`: run `wt dev stop` unless you still plan to use the server, and
   do any step you owe now that it merged. A step that needs a later event
   is a wake whose message is the step itself
   (`wt wake production -m "<the step>"`). A check is never a wake; wt wakes
   you for it. Then end the turn and say it merged.
4. `evicted`: see below. `closed`: someone closed the PR; stop and say so.

A `pr.merge_dropped` rule may prompt this session about the same eviction
(`wt automations` shows what runs); it is one eviction, not two. If you had
to hand a long wait to a wake (see *The loop*), make it
`wt wake 1h -m "babysit: not merged yet? read the row for an eviction"` and
say the PR is armed, not merged. Woken unmerged, read the row and go back to
step 2. Woken after the merge, close it: `wt done <id> -m "merged"`.

When `wt wait` says `evicted`, or the PR is open and not draft but has no
`.merge_queue` entry and `.pr.auto_merge` is false, or its queue entry reads
`UNMERGEABLE`, it was evicted. Find out why before acting. `wt wait` prints
GitHub's reason on the `evicted` line when GitHub gave one (`removed from the
merge queue: <reason>`), and `wt fleet --json` carries it as
`.queue_removal`. Then read the failed queue
run (`gh pr checks`, then its log), a conflict, or a new requirement. Fix it
through the normal loop (findings, push, review), then `wt merge` again.
Never re-arm unchanged after a real failure; it fails the same way. A flaky
queue run with no code cause may be re-armed once; a second failure is a
real one.

## After it lands

Merging alone brings you back for nothing. What does:

- A check (`--verify-in-staging`, `--verify-in-production`; both may be
  owed): wt wakes you when the change is deployed there; do not poll for it.
  Confirm the deploy carries the change, run the steps, then
  `wt done <stage> -m "<what you checked, where>"`. If you cannot run it
  yet, `wt done <stage> --again <time>`, never a separate `wt wake`.
- A step you set a wake for comes back as a due step: do it, then
  `wt done <id> -m "<what you did and saw>"`, or `--again <condition>` to
  come back for another part.
- Do not remove the worktree; merge cleanup archives it.

Your part ends when the PR has merged and the steps in *Until it lands* are
done, when a genuinely long wait was handed to a wake, or when blocked on a
human. `wt owed` lists what still brings you
back. Never keep waiting on a clean current-head review; merge it.

## Next PR from this worktree

When asked for more work after the merge, run
`wt next --title "<outcome>"` (`--issue <ID>` alone takes the tracker's title;
add `--issue none` or `--gh <N>|none` to change the links): it moves the
worktree to a fresh branch from the latest base and keeps its sessions,
section and history. Then `wt status working`, commit,
`git push -u origin HEAD`, open the PR and run this loop. Never create the
next branch in place or reset the merged branch onto the base by hand: wt
refuses a PR from a branch it did not record, and after a reset the merged PR
stays attached and the row can read as landed. Work that must stack on this PR
before it merges goes to a new worktree (`handoff`), never a second PR here.
