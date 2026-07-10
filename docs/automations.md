# Automations

`[[automations]]` bindings run an action (or a built-in flow) automatically when a condition holds for a worktree — CI failed, a review requested changes, a stack parent merged. Nothing is automated by default; the section is strictly opt-in. Config fields and examples: [configuration.md](configuration.md#automations--optional-strictly-opt-in).

This page explains the runtime semantics — what "fires", when, and why it won't fire twice. (For contributors: the engine is `core/automations.ts` for the persistent ledger, `tui/automation-rules.ts` for pure condition evaluation, and `tui/hooks/useAutomations.ts` for the queue + dispatch loop.)

## Level-triggered, ledger-deduped

Rules are **level conditions, not edge events**. On every evaluation pass the engine re-derives "does this trigger hold for this worktree right now?" from the same row state the TUI renders. Firing once-per-instance comes from a persistent ledger of *fire keys* (`~/.cache/wt/automations.json`), not from watching transitions — so a TUI restart can't replay a fire, and a missed edge can't lose one.

Fire keys embed the PR's head SHA where relevant: a new push produces a new key (the same failure on new code fires again), while re-evaluating the same failed state does nothing.

## Triggers

| trigger | holds when |
|---|---|
| `pr.checks.failed` | the open PR's checks rollup is failing |
| `rabbit.unresolved` | CodeRabbit has unresolved review threads |
| `review.changes_requested` | a human review requested changes |
| `pr.conflict` | the merge-tree probe says the branch conflicts with its effective base |
| `wt.merged` | a non-stacked worktree's branch landed (merged / upstream gone / PR merged — the same set the `c` clean sweep uses) |
| `stack.parent_merged` | a stack (worktrees chained by their recorded fork bases — see [stacked-prs.md](stacked-prs.md)) has a merged member with open members stacked on it |

PR-driven conditions additionally require a **live GitHub fetch this session** — data restored from the persisted cache never fires a rule — and a known `pr.headRefOid` to key the fire against.

## Dispatch pipeline

When a condition holds and its fire key is unseen, the rule creates an **intent** in an in-memory queue (deliberately not persisted; it rebuilds from conditions on the next boot). Delivery then waits for:

- **Settle window** (`settle_seconds`): the intent must be at least this old AND the worktree free of edits for this long. This is also your window to cancel by just… doing something in the worktree. Merge triggers default to 10s (a merge can't un-happen); everything else defaults to 120s to ride out CI/review churn.
- **Quiescence**: if the worktree has a live session that's working or asking, or an action already running, the `busy` policy decides — `queue` (default) holds the intent until things settle, `skip` marks the fire handled and drops it. There is deliberately no "force": injecting into a session that's asking a permission question would answer the dialog with the paste's trailing Enter.
- **Cooldown** (`cooldown_minutes`): minimum spacing between dispatches per (rule, worktree).

Dispatch goes through the exact same paths keystrokes use (`launchAction`, the clean flow, the restack flow) — automations have no special powers.

## Failure handling and the breaker

- A dispatched run that fails is marked delivered and **never retried**. The retry is a new head SHA (push a fix, the condition re-fires under a new key).
- A per-(rule, worktree) **circuit breaker** counts consecutive dispatches after which the condition still held. At 2, the rule trips for that worktree and stops dispatching; it re-arms only when the condition is observed false (i.e. something actually got fixed, by hand or otherwise).

## Pausing

- `A` toggles a global pause of all automations.
- `Ctrl+A` pauses the selected worktree — or its whole stack when it's a stack member. A stack pause is stored both under the stack's id (the root branch — covers members stacked on later) and as per-member flags (covers the survivors when the root lands and the stack re-roots under a new id).

One identity caveat: the circuit breaker and cooldown for `stack.parent_merged` are keyed by the stack id, which changes when the root lands and is cleaned — their accumulated state starts fresh for the re-rooted stack.

Both persist across restarts. Paused rules still evaluate (so state stays current); they just don't dispatch.
