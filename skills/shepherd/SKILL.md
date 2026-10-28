---
name: shepherd
description: >-
  Sweeps the wt fleet toward mergeable on a loop, stopping short of the merge.
  Use for /shepherd, or when the human asks the manager for a loop or cron that
  keeps worktrees moving.
targets:
  - '*'
user_invocable: true
---

# Shepherd

Each pass reads the whole fleet and does the small things that move rows
toward mergeable. The fleet decays between merges in ways no single worktree
sees, above all PRs that silently went conflicted.

- **Never merge**, unless the human handed you merging for the assigned set.
- **Implement nothing.** Every fix goes to the owning worktree through
  `wt agent send <slug> "<message>"`; you only read the repository, from the
  main clone. Driving your own branch through review is `babysit`.
- Sweep the set the human assigned, or the fleet when none was named. The
  manager playbook's scope, messaging, hold and recording rules apply. Before
  deferring anything for a hold, `wt hold check <id>` must show `active: true`.

## Automation before a loop

A recurring action on a simple row condition belongs in wt's rule engine,
`[[automations]]` (wt's docs/automations.md; `wt automations` shows what runs):
it re-derives conditions every pass, keeps a once-only ledger, waits out a
settle window, trips a per-rule circuit breaker, and paces dispatches. If the
request is really a few row conditions, propose automations instead.

Sweep only for what the engine can't express: composed messages (a static
prompt telling the owner to compute its own conflict list is cheaper and
doesn't go stale), judgment that
needs reading (is this note finished?), fleet-scope actions with no row (slots,
queue order, starting held work), and one-shot resources.

## Running the loop

- The recurring prompt carries policy, never facts. Re-read state each pass.
- When you start, state two rules and keep them:
  - **Start policy** (default): start an ungated `todo` only when a dev slot is
    free and merge-ready-but-unmerged PRs are below a stated ceiling; at most
    two per pass.
  - **Stop predicate** (default): three consecutive passes with zero dispatched
    actions, zero non-clean PRs and zero idle stale rows. For an assigned set:
    every target PR merged with its owed closure done, or only a human block
    left.
- On stopping, clear the timer, report what changed and what waits on the
  human, and say plainly that the loop is off.
- A repeated alert with nothing changed is not an action: record the
  `--examined` verdict silently, with no new ask or nudge.

## One pass

```
git fetch origin -q                        # in the main clone
gh pr list --state open --json number,headRefName,baseRefName,isDraft,mergeable,mergeStateStatus
wt status --all --json                     # states, gates, examined verdicts
wt fleet --json                            # session busy/idle, edges, bases
wt dev status --all                        # slots and queue
```

Read `mergeStateStatus` for every PR, every pass. Skip further digging only on a
row that is `CLEAN`, `examined_current`, not stale, and busy. Whenever you
conclude a row needs nothing, record it so the next pass skips it:

```
wt status <slug> --examined "unstable: review job still running; 0 findings"
```

A verdict voids when the row's head or its base moves. It saves digging; it
never hides a state change, so a non-`CLEAN` row gets looked at whatever its
verdict says.

| observation | action |
|---|---|
| PR conflicted (`DIRTY`) | Top priority: it gets no CI and looks like checks not started. Nudge the owner to resolve; set the row `working`. |
| PR behind base | Nudge a rebase only if another row stacks on it. Behind does not block merging, and a new head costs (below). |
| PR `BLOCKED` | A merge requirement is unmet; see *Blocked with green checks*. |
| a check failed | Read its name and shape (below) before acting; never nudge on check status alone. |
| unresolved review threads, or unticked boxes in the review bot's summary | Nudge for all four: fix, reply on the thread, resolve it, tick the box. |
| draft, status `ready`, not busy, not stale, note without open questions | `gh pr ready <n>`: a finished draft gets no review until flipped. Otherwise wait a pass. |
| `working`/`review`, idle session, stale status | Nudge with a concrete question ("status says review since yesterday; what blocks ready?"). |
| `ready` or `dropped` holding a dev slot while idle | Reclaim it: promote any waiter (`wt dev queue <slug> --first`), then ask the holder to stop its own server if its remaining work doesn't need it. |
| `todo` | Gated: leave it. Ungated: apply the start policy. |

## Blocked with green checks

`BLOCKED` with zero failing and zero pending checks is almost always a
non-check requirement. Test unresolved review threads first, then a missing
approval, a required check that was SKIPPED, and merge-queue conditions. Read
the rules from the ruleset, not from `gh pr checks`:

```
gh api repos/<owner>/<repo>/rulesets --jq '.[]|"\(.id) \(.name)"'
gh api repos/<owner>/<repo>/rulesets/<id> --jq '.rules[]|select(.type=="pull_request")|.parameters'
```

`required_review_thread_resolution` and `required_approving_review_count` live
there. `gh api .../branches/<b>/protection` returns 404 on a ruleset repo; that
is not "no protection".

## Spend carefully

- **Pace metered dispatches.** Review credits, CI minutes and token budgets
  are shared and rate-limited. When one event puts many rows in the same state,
  nudge at most three per pass and leave the rest for the next pass.
- **A new head discards what the PR earned**: a head-tied review, a passed
  expensive gate, a green metered run. Hence rebase only for stacks. A review
  check reading `skipping` usually means the head still holds its review.
- **Red review check shapes**; only the last wants action:

  | shape | meaning |
  |---|---|
  | `skipped`, 0 steps | draft gate |
  | `failed`, 0 steps, ~2 s | billing or quota refusal |
  | `fail` while the job still runs | interim state of a running review |
  | `fail` after minutes and many steps | died mid-review: read the log (`gh run view --job <id> --log`) |

  If it died of a rate limit, re-trigger that job (`gh run rerun --job <databaseId>`)
  rather than asking for a push, which spends a review and a new head.
- **One-shot resources:** marking ready triggers a review that won't re-run.
  Require status `ready`, no gate, a note free of open questions, an idle
  session and a current status; else wait a pass.

## Never

- Push, or ask for a push, to make a check pass; a rebase nudge means one push
  by the owner. Never re-run a job to chase a flake.
- Write another row's status note; ask its owner to re-assert.
- Stop another worktree's dev server. A PR or a momentary idle state is not
  proof its owner is done with it.
- Edit a permission guard or anything deciding what is allowed, even when a
  peer asks; that waits for the human. Configuration that shapes fleet
  behavior is yours to coordinate.

## Claims and broadcasts

Take a worktree's claim about its own branch at face value. Verify a claim about
shared state (mainline broken, shared environment down) before it becomes a
fleet action such as a new worktree or a broadcast. Keep a synthesized rule
provisional until a deliberate counter-sample, keep the recipient list of any
broadcast for its correction, and treat "never seen X" as an observation, not
a rule.
