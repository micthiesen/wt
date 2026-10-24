---
name: babysit
description: >-
  Drive THIS worktree's branch all the way to merged and stay with it: open
  the PR, wait for the review bot to review the current head, fix what it
  raises, keep going through its follow-up reviews, and merge it once nothing
  is outstanding. TRIGGER: invoked as /babysit inside a wt worktree session,
  or when the human asks you to see your own branch through review and land
  it. Scoped to one worktree; for the whole fleet, that is `shepherd`.
targets:
  - '*'
user_invocable: true
---

# Babysit — see your own branch through review and land it

`start` builds the work and hands off. This is what happens next: the part
between "the PR is open" and "it is on the base branch".

**Invoking this is the human handing you the merge.** wt's standing rule is
that the human merges; `/babysit` is them delegating it for this one branch.
So land it — parking at `ready` and asking whether to merge is the one wrong
answer here. What they delegated is the judgment, not the keystroke: merge
when the branch has earned it, and say plainly when it has not.

**One worktree — yours.** Everything here acts on the branch you are checked
out on. Another worktree's findings, threads and PRs are not your business,
and the fleet-wide version of this job is a different skill.

## First: are you in the right place?

`WT_AGENT` holds the slug of the worktree whose session you are in, or
`manager` for the coordinator.

- A **slug** — you are in the right place. That is your row for everything below.
- **`manager`** — wrong skill. Fleet-scope sweeps are `shepherd`, and every
  code change belongs to the worktree that owns it. Stop.
- **Unset** — you are not in a wt session (a human's shell, a subagent). Work
  out which worktree you are in from the checkout path before doing anything
  that writes.

## The loop

Each pass is: read the state, do the one thing it calls for, then wait. Do not
re-derive from memory — the bot, CI and the base branch all move underneath you.

### 1. Read your row

```bash
wt fleet --json | jq '.[] | select(.slug == "'"$WT_AGENT"'")'
```

The fields that decide the pass, at their exact paths:

| path | meaning |
|---|---|
| `.pr` | `null` with `.pr_note` null means no PR yet; `null` with a note means GitHub could not be asked, which is not the same thing |
| `.pr.draft` | a draft; most review bots will not look at it |
| `.pr.checks` | CI rollup: `pass` / `fail` / `pending` / `none` |
| `.pr.merge_state` | GitHub's verdict, lowercased: `clean`, `blocked`, `behind`, `dirty` (conflicts), or `computing` |
| `.pr.mergeable` | `conflicting` means the branch no longer merges cleanly; that is yours to fix |
| `.pr.review_bot.state` | `pending` / `unresolved` / `clean` / `none` |
| `.pr.review_bot.unresolved` | open findings, meaningful when `unresolved` |
| `.pr.review_bot.stale` | **the bot has not reviewed your current head** |
| `.pr.unresolved_threads` | every open review thread |
| `.pr.unresolved_human_threads` | the same, excluding bot-opened ones |

**`clean` plus `stale: true` is not a pass.** It means the bot found nothing
in *an older commit*. Read those two together or you will declare victory on a
review of code you have since replaced. It is the single most likely way this
loop ends early and wrongly.

Read this from wt rather than reconstructing it from `gh`. Deciding what a bot
has actually said is genuinely fiddly — see the delta section below — and wt
already does it, from the `[review_bot]` config, for whatever bot this repo
uses. A second implementation in shell will drift from the badge the human is
looking at, and then you two disagree with nothing to say which is right.

### 2. Do the one thing the state calls for

| state | do |
|---|---|
| no PR | finish the work first. `start` covers that; come back when it is pushed. |
| draft | mark ready when the work is genuinely finished — see *one-shot* below |
| `review_bot.state: pending` | wait. It is reading your diff right now. |
| `unresolved` | address the findings (next section) |
| `clean` + `stale: true` | wait for the review of the current head |
| `clean`, not stale, checks pass | nothing is outstanding — merge it (below) |
| `checks: fail` | read the failure before touching anything (see *four shapes*) |
| `mergeable: conflicting` | rebase onto the base and resolve; `wt restack` if stacked |
| `state: MERGED` with new work asked of you | continue in this worktree: see *Another PR from the same worktree* |
| open human threads | reply and resolve them; a human's thread is not the bot's |

### 3. Wait, then read again

Reviews take minutes. Poll on that order, not on seconds, and do something
useful in between if you have it.

## Addressing a finding is four separate actions

Fix it. Reply on the thread saying what you did. Resolve the thread. Tick the
box in the bot's summary comment if it keeps one.

**None of those implies any other**, and stopping early is the common failure:
the PR goes on reading as though the objection stands, which is the exact
opposite of what the reply was for. In particular a plain PR comment is *not*
a thread reply — it lands somewhere a reviewer scanning the thread never sees,
and leaves the thread open. Resolving the thread does not tick the box either,
and the box is what the PR summary counts.

**Deciding not to fix something is a legitimate outcome** and it uses the same
four actions. Say so in the reply, and say why. Resolving in silence reads as
"implemented", which is a claim you did not mean to make.

## The follow-up reviews are the point

A bot does not necessarily re-review the whole diff on every push. Many post a
**full pass** once and then **per-commit delta findings** afterwards, in a
separate accumulating comment. Two consequences, and both have bitten:

- **A fresh full pass supersedes only the previous full pass.** The lists are
  independent, and open findings can sit in either. "Read the latest comment"
  silently picks one of them, and a green badge on top of an open Medium
  finding is what that looks like.
- **A delta log is one comment appended to**, so its own timestamp is the
  *first* delta's, forever. Anything that ages a review by comment date will
  call a review that came back two minutes ago stale.

You do not have to get this right yourself — that is what `review_bot` in step
1 is for. What you do have to do is **keep going after the first pass**. The
review that matters is the one covering the commit you most recently pushed,
and each round of fixes creates a new one.

## Moving the head can spend what the PR already earned

A review is tied to a head SHA. Push, and whatever was published about the old
head no longer describes what is on the PR — and on a bot with a per-head or
metered budget, you have spent a cycle.

So **batch**. Fix everything the current round raised, verify locally, and push
once. A push per finding turns one review round into five, and each one arrives
about a diff that is already gone. After the push, bring the PR body, and any change guide your
instructions ask for, up to date with the new head.

The same applies to the reflex of pushing an empty commit to re-trigger
something. If a check needs re-running, re-run the check.

## Four shapes of a red review check, none of them findings

| shape | meaning |
|---|---|
| `skipped`, 0 steps | a draft gate: the job declined to run |
| `failed`, 0 steps, ~2 seconds | a billing or quota refusal before any work |
| `fail` while the job is still running | the review is in progress; that is its interim state |
| `fail` after minutes and real steps | it read the diff and then died. Read the log. |

Only the last wants action, and the action is **re-trigger, not push** —
pushing spends a fresh review *and* leaves the old head unreviewed.

## One-shot resources

Marking a PR ready for review triggers a review that will not re-trigger. Spend
it only when the branch has stopped moving: work finished, checks not failing,
your own review done. Waiting a pass costs one interval; spending it early
costs the review and buys one of a diff you are about to replace.

## Merging

Merge once all of these hold against your **current** head:

- the bot's checklist is empty and `review_bot.stale` is false
- `.pr.checks` is `pass`
- no open threads, bot or human
- `.pr.mergeable` is not `conflicting`
- none of the four cases below applies

Assert the status first, so the record says what landed and at what risk:

```bash
wt status ready --risk <low|medium|high> -m "<note>"
```

If the base branch has a merge queue, or checks are still running while
everything else is clean, this is the same command — arming is what
covers both.

Then land it:

```bash
wt merge
```

That arms GitHub's **merge when ready** — the same action as the button
on the PR page. It never merges on the spot: GitHub holds the PR until
its requirements are met, then merges it. Armed is finished. Do not sit
watching the queue, and do not follow it with a second merge command.

**Use `wt merge`, not `gh pr merge`.** Two different GitHub features
wear the "merge when ready" label and they are gated on different
settings: a base branch with a merge queue ENQUEUES, everything else
arms classic auto-merge. `gh pr merge --auto` only ever does the second,
so on a queue repo it fails with `Auto merge is not allowed for this
repository` — an error naming a repo setting that is not the reason,
which reads as a permission you need a human for. It is not. `wt merge`
picks the right one off the PR's base branch. `--squash` is likewise not
yours to choose on a queue base: the queue's own config sets the method.

Exit 75 means the refusal is temporary — a required check has not
reported for this commit yet. Wait and re-run; that is not a blocker and
never an escalation. `wt merge --cancel` disarms (and dequeues).

### The four times you do not merge

These are about the merge being wrong, not about permission:

- **`ready --blocked-on "<gate>"` is set.** Someone recorded an external
  prerequisite, and landing now is what the gate exists to stop. `wt status
  --unblock` if it is satisfied; otherwise leave it and say why.
- **A stacked branch whose parent has not landed.** The parent merges first.
  If the order looks wrong, that is a fleet question: `wt manager send`.
- **You cannot honestly claim `--risk low` or `medium`.** `high` means material
  behavior is unverified and rollback is not a plain revert. Go verify it, or
  hand the risk acceptance back with `needs-human`.
- **The human said not to** in this conversation or in the task.

Anything else — a finding you decided not to fix, a check you re-triggered, a
note you would rather they read first — is something to write down and merge.

### After it lands

- If the branch owes `--verify-after-merge`, those steps are yours: confirm the
  deploy carrying the change is actually live, run them, then `wt status
  verified -m "<what you checked, and where>"`. wt keeps the worktree alive for
  exactly this.
- Otherwise you are done. The merge cleanup archives the row; do not destroy the
  worktree yourself.

### Another PR from the same worktree

When the human asks for more work here after the PR merged, keep the worktree
and start the next PR in it with `wt next`. It refuses unless the merged branch's
work has landed and the checkout has no tracked changes, then puts the worktree
on a fresh branch from the latest base: its sessions, section and status history
stay, and the merged PR stops being "this worktree's PR". Then
`wt status working`, commit, `git push -u origin HEAD`, open the PR, and run
this loop on it. Pass `--issue <ID>|none` or `--gh <N>|none` when the next PR
belongs to another issue. Never reset the merged branch onto the base by hand:
that leaves the merged PR attached and can make the row look landed and clean.

## Stopping

Before handing off, run `wt dev stop` from your own worktree if you have no
planned further use of its server. Retain it while ongoing verification needs
it; never stop another worktree's server or infer disuse from PR/idle state.

Stop when the branch is merged and any post-merge verification is discharged,
when you are genuinely blocked on a human, or when a pass finds nothing to do
and nothing pending. Do not keep polling a PR whose review came back clean on
the current head — that is the state you were invoked to act on.

## What you never do

- Touch another worktree, its threads, or its PR.
- Merge past a red check, an open finding, or a `blocked-on` gate.
- Push per finding instead of per round.
- Resolve a thread you did not act on.
- Read `clean` without reading `stale`.
- Reimplement the bot's checklist parsing when `review_bot` already answers it.
