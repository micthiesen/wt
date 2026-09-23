# Automations

`[[automations]]` bindings run an action (or a built-in flow) automatically when a condition holds for a worktree — CI failed, a review requested changes, a stack parent merged. Nothing is automated by default; the section is strictly opt-in. Config fields and examples: [configuration.md](configuration.md#automations--optional-strictly-opt-in).

This page explains the runtime semantics — what "fires", when, and why it won't fire twice. (For contributors: the engine is `core/automations.ts` for the persistent ledger, `tui/automation-rules.ts` for pure condition evaluation, and `tui/hooks/useAutomations.ts` for the queue + dispatch loop.)

## Level-triggered, ledger-deduped

Rules are **level conditions, not edge events**. On every evaluation pass the engine re-derives "does this trigger hold for this worktree right now?" from the same row state the TUI renders. Firing once-per-instance comes from a persistent ledger of *fire keys* (`~/.cache/wt/automations.json`), not from watching transitions — so a TUI restart can't replay a fire, and a missed edge can't lose one.

Fire keys embed the PR's head SHA where relevant: a new push produces a new key (the same failure on new code fires again), while re-evaluating the same failed state does nothing.

## Triggers

| trigger | holds when |
|---|---|
| `wt.created` | a successfully completed worktree creation has a recorded creation timestamp. The rule is opt-in and does not backfill older worktrees without that timestamp. External bookkeeping shell actions may run while the harness is busy; removed worktrees and per-worktree pauses still suppress the fire |
| `pr.checks.failed` | the open PR's checks rollup is failing |
| `review_bot.unresolved` | the configured [`[review_bot]`](configuration.md#review_bot--the-bot-review-track) has unresolved findings — unresolved threads (CodeRabbit) or unticked checklist boxes, per `unresolved_via`. `rabbit.unresolved` is accepted as a legacy alias |
| `review.changes_requested` | a human review requested changes |
| `pr.conflict` | the merge-tree probe says the branch conflicts with its effective base |
| `wt.merged` | a non-stacked worktree's branch landed (merged / upstream gone / PR merged — the same set the `c` clean sweep uses). A branch with no commits of its own never counts as landed, however contained it looks — see the vacuous-containment guard in `branchIsMerged`. Rules running `builtin:close-issue`, `builtin:delete-branch`, or a shell action declared `external = true` also evaluate stack members — those runs never touch the worktree, so the clean-vs-restack race the exclusion protects against doesn't apply |
| `stack.parent_merged` | a stack (worktrees chained by their recorded fork bases — see [stacked-prs.md](stacked-prs.md)) has a merged member with open members stacked on it |
| `status.verification_overdue` | the branch has LANDED, still owes a [`--verify-after-merge`](cli.md#--verify-after-merge-steps--the-check-that-can-only-run-once-it-ships) check, and has owed it for longer than the rule's `after_days` (default 2). **The one trigger here that fires repeatedly**: its fire key carries the local day, so it re-fires once a day until someone asserts `verified`. That is deliberate and is not the echo problem the `by` stamp exists for — a daily reminder is only noise when the recipient can clear it by writing something, and nothing clears this except the check actually happening. Mostly local (wtstate + merged/gone), but the PR-merged leg of "landed" is gated on GitHub freshness so a boot-stale cache cannot nag about a merge it has not confirmed. Settle defaults to 0: the condition is days old by the time it holds, so a window measured in minutes would only delay a reminder that is already late |
| `branch.advanced` | a watched branch's tip MOVED. **Fleet-level: it targets no worktree**, which is the whole point — by the time a release branch advances, the worktrees whose work is in that range have been swept, so there is no row to key a per-row condition against. Requires `branch = "<name>"`, which must be `[branch] base` or in `[branch] keep_fresh` (the loader refuses otherwise: nothing else advances a local head, so watching one wt never fetches is a rule that can never fire, and a rule that never fires is indistinguishable from a working one). The run launches in the **main clone** with `{{branch}}`, `{{from}}` and `{{to}}`; walking the range is the run's job. **First sight fires nothing** — with no recorded tip there is no range, and reading an absent watermark as "everything up to here" would fire once across the branch's entire history. wt records the tip and waits for the next move. The watermark advances only on DISPATCH, so a declined launch keeps its range instead of dropping it. The per-worktree breaker does not apply to this fleet trigger: each tip is a distinct event, and no worktree condition can clear its breaker |
| `status.needs_human` / `status.needs_testing` / `status.ready` | the worktree's asserted [work status](cli.md#wt-status-slug-state--m-note---risk-r) is that state. Local (wtstate), so no GitHub-freshness gate; the fire key carries the assertion timestamp, so one assertion fires once and re-asserting fires again — unless the re-assert is identical (same state/note/risk/sha), in which case `setSlugWorkStatus` is a no-op that keeps the original `at`, so it doesn't refire, or the asserter is the session the rule would brief (below). Hyphenated spellings (`status.needs-human`) are accepted aliases. **`status.ready` does not fire while the record carries a [`--blocked-on` gate](cli.md#wt-status-slug-state--m-note---risk-r---blocked-on-gate)**: every documented use of it says "this is yours to merge", and firing that at a branch that must not be merged is the misread the gate exists to prevent, amplified to a macOS banner. The fire is not lost — `--unblock` amends in place, so `at` is unchanged and the fire key was never consumed. Settle defaults to 0 — an assertion is a deliberate write, not flappy derived state |

PR-driven conditions additionally require a **live GitHub fetch this session** — data restored from the persisted cache never fires a rule — and a known `pr.headRefOid` to key the fire against.

## A briefing never echoes its own audience

A `status.*` rule whose run is a prompt action aimed at a live session (`target = "manager"` or `"session"`) does not fire when the record's `by` says that same session asserted it. Everything else is unaffected: `builtin:notify` talks to the human, `builtin:clean` and `builtin:restack` talk to git, and a `headless` run is a fresh conversation that wrote nothing, so none of them has an audience to echo back at.

This exists because the manager's *last* triage step is sharpening the `needs-human` note it was briefed about. That re-asserts the state, which mints a new timestamp, which mints a new fire key, which briefs the manager again — quoting its own words back and asking it to triage them. Observed three times for one slug, and the honest answer to the third was "nothing changed".

The wasted turn is not the cost. A briefing whose correct answer is usually "nothing changed" stops getting read, and that spends the one channel that exists for "a worktree is blocked on the human". Note that only `status.*` triggers can loop this way — every other condition is derived from git or GitHub, which no session can write by asserting.

The guard is deliberately narrow in the other direction too: an **unattributed** record (the human's `u` picker, a `wt status` typed in a plain shell, or any record written before `by` existed) is not the audience, so it still fires. A guard keyed on stored state has to decide what a missing value means, and here "unknown" must mean "brief it" — the alternative silently swallows real escalations on exactly the rows that have no stamp.

## Built-in runs

`run` names either an `[[actions]]` id or a built-in flow:

- `builtin:restack` — the squash-safe stack replay (requires `on = "stack.parent_merged"`).
- `builtin:clean` — destroy one landed worktree (the `c` sweep for a single row). Inherits the sweep's refusal to force: a worktree holding uncommitted changes or unpushed commits is kept and narrated on the attention feed instead of destroyed. That guard matters most here, where nothing prompts and nobody is watching — "the branch landed" is a fact about the branch, and this deletes a directory.
- `builtin:notify` — a macOS `display notification` banner titled `wt · <slug>` carrying the trigger detail (for status triggers: state, risk, note). Notifications never touch the worktree, so they bypass the quiescence gate — a `status.needs_human` fire happens exactly while the session is busy asking. Pairs with any trigger; the canonical use:

```toml
[[automations]]
id  = "ping-needs-human"
on  = "status.needs_human"
run = "builtin:notify"
```

- `builtin:close-issue` — close the worktree's attached GitHub issue as completed once its branch lands (requires `on = "wt.merged"`; merges via GitHub and via wt look identical to the level condition; one rule max — a second would just race the first). The issue is the `--gh <n>` secondary id, or a `GH-<n>` primary slug id on repos without a tracker; a merged worktree with neither simply never fires. This is the deliberate replacement for GitHub's PR-body `Closes #N` keywords on repos whose feature PRs merge into a non-default branch (e.g. `staging`), where those keywords never fire — and where they do fire, losing the race is fine: failures ("already closed", anything else) are logged as advisory and never retried. A successful close narrates on the ATTENTION feed rather than the firehose (and toasts): it is the one builtin that writes to a system outside wt, where wt's undo doesn't reach and where the board shows nothing. Four deviations from the standard pipeline, all following from "it never touches the worktree and a merge can't un-happen": it also fires for stack members (see the trigger table), it bypasses quiescence entirely, it's breaker-exempt like `builtin:notify` (closing an issue doesn't clear the merged condition — only cleanup does), and its queued intent survives the row's death — the issue number is frozen into the fire at evaluation, so a clean that razes the worktree before delivery (a restack pre-clean, or a manual `c` inside the settle window) doesn't lose the close. Detaching the issue while the row is still alive, or pausing the worktree, still cancels a queued close; a merge that happens entirely while wt isn't running and whose worktree is also cleaned before the next launch is the one case nothing can observe.

```toml
[[automations]]
id  = "close-issue-on-merge"
on  = "wt.merged"
run = "builtin:close-issue"
```

- `builtin:delete-branch` — delete the branch's ref on the origin repo once it lands (requires `on = "wt.merged"`; one rule max — a second would race the first to a DELETE that can only succeed once). This is GitHub's **"Automatically delete head branches"** setting, for repos that have not enabled it, and it exists for the same reason `builtin:close-issue` does: the repo-level feature is not always available to you, and the landing is a signal wt already computes.

  **It confirms the merge against GitHub before deleting, and declines otherwise.** The fire carries the branch AND the PR number it is claiming merged, both frozen at evaluation; at dispatch a live `gh pr view` on that branch must come back `MERGED`, or — for a fire whose evidence was local containment with no PR at all — come back with no PR. Anything else declines onto the attention feed. This is deliberately a second check on a conclusion wt already reached, because the effect is the only one in the engine wt cannot undo, and it is worse than it looks: **GitHub CLOSES any open PR whose head ref disappears, and files no close event explaining it.** So a wrong delete destroys a PR and simultaneously hides the cause — the incident that produced this guard had the worktree agent and the human both concluding a person or a bot had closed #1631, because from GitHub's side that is exactly what it looks like. The condition that fired it had been true for a *previous* branch: `merged`/`gone` were cached per slug rather than per branch, so repointing a row served the old branch's verdict, and the fire key (`<rule>:merged:<slug>:<pr>`) changed with the new PR number and let it fire a second time. The keys are branch-scoped now; the confirmation is what makes the consequence unreachable from whatever causes the next one.

  It is the second builtin that writes OUTSIDE wt, so it inherits close-issue's whole shape and for identical reasons: it fires for stack members too, bypasses quiescence, is breaker-exempt (deleting a branch cannot clear the merged condition — only cleanup does), survives the row's death because the branch name is frozen onto the fire at evaluation, and narrates a success on the ATTENTION feed rather than the firehose. `isPostMergeExternalFire` in `hooks/useAutomations.ts` is where they share that behaviour; a future post-merge run that *does* touch the worktree must not join it.

### Post-merge runs that leave the repository

The four deviations above are not builtin-only. **A `wt.merged` rule running a shell action declared `external = true` gets all four**, because `external` already means exactly the property they follow from: the effect leaves the repository, so the run needs nothing from the checkout and must outlive it. Such a fire carries an empty `quiesceSlugs`, is breaker-exempt, evaluates stack members, and survives its row's death — its template vars (`{{issue_id}}` above all) are frozen at evaluation and it dispatches in the **main clone**, the only checkout guaranteed to still exist. A frozen fire whose `requires` are unmet is dropped rather than kept pending, since by construction nothing about it can change.

This generalization is a bug fix, and the bug is worth keeping in view: the exemption used to be a hand-written list of two run ids, so a config action with all three properties got none of the three exemptions. Every merge queued the tracker transition and every merge dropped it again — `superseded (condition cleared)` — because the `c` sweep archived the row roughly 7 seconds after the landing, inside the 10-second merge settle window. `builtin:delete-branch`, queued in the *same millisecond*, ran fine three seconds later. Two intents born of one event disagreeing about whether their subject still exists is the tell that the exemption is keyed on identity rather than on the property.

Prompt actions are deliberately excluded even when `external`: they are delivered into a session **in the worktree**, so the checkout is precisely what they do need.

  Two things are specific to this one. Failure is usually not failure: a repo with GitHub's own setting on, or anyone deleting by hand, wins the race and GitHub answers `Reference does not exist`, which is the end state we wanted — logged as advisory, never retried. And it refuses `branch.base` outright. No worktree branch is ever the trunk, so that guard is defence in depth rather than a live worry; it is there because this is the one mutation in the codebase whose blast radius is the repo's mainline rather than a retry.

  Deleting a merged parent's ref is safe for a stack on both sides: GitHub retargets an open child PR onto the deleted base's own base, and wt's restack replays from the `baseSha` anchor in wtstate, never from the remote ref. It is also already accounted for locally — `localOnlyCommits` treats "no `origin/<branch>` and the branch landed" as a squash merge's pre-squash originals, not as unpushed work, so the destroy guards do not start crying wolf once the ref is gone.

```toml
[[automations]]
id  = "delete-branch-on-merge"
on  = "wt.merged"
run = "builtin:delete-branch"
```

## Dispatch pipeline

When a condition holds and its fire key is unseen, the rule creates an **intent** in an in-memory queue (deliberately not persisted; it rebuilds from conditions on the next boot). Delivery then waits for:

- **Settle window** (`settle_seconds`): the intent must be at least this old AND the worktree free of edits for this long. This is also your window to cancel by just… doing something in the worktree. Merge triggers default to 10s (a merge can't un-happen); everything else defaults to 120s to ride out CI/review churn.
- **Quiescence**: if the worktree has a live session that's working or asking, or an action already running, the `busy` policy decides. `queue` (default) holds the intent until things settle, while `skip` marks the fire handled and drops it. There is deliberately no "force" so automations do not collide with an active or blocked turn, regardless of harness transport.
- **Cooldown** (`cooldown_minutes`): minimum spacing between dispatches per (rule, worktree).

The queue event includes any remaining settle delay. A queued review or CI
remediation normally waits two minutes before delivery; it may wait longer for
an idle session or another dispatch to finish. Pausing and resuming starts a
fresh settle window for conditions that still hold.

Dispatch goes through the exact same paths keystrokes use (`launchAction`, the clean flow, the restack flow) — automations have no special powers.

## Failure handling and the breaker

- A dispatched run that fails is marked delivered and **never retried**. The retry is a new head SHA (push a fix, the condition re-fires under a new key).
- A per-(rule, worktree) **circuit breaker** counts consecutive dispatches after which the condition still held. At 2, the rule trips for that worktree and stops dispatching; it re-arms only when the condition is observed false (i.e. something actually got fixed, by hand or otherwise). Fleet `branch.advanced` fires bypass it because no row can provide that clearing observation. A fire suppressed by a breaker or another terminal pre-dispatch guard is recorded as skipped, so it does not requeue on every pass.

## Pausing

`Ctrl+Shift+A` clears all currently queued automation intents in this TUI.
Their exact fire keys are recorded as cancelled in the persistent ledger, so
the same instances do not return on the next poll or restart (subject to the
ledger's 30-day retention). New trigger instances can still queue normally.
This does not pause rules, change cooldowns or breakers, stop running actions,
or recall prompts already handed to a harness's native queue. Unlike clearing,
pausing drops pending intents without consuming their keys, so they can return
when resumed.

Clearing also works while globally paused, without resuming or dispatching
anything. It reconstructs currently eligible, unseen trigger instances from
the same evaluator and cancels those exact keys. Live GitHub freshness,
per-worktree pauses and row-eligibility guards still apply; this does not
recover historical instances whose rows or conditions have disappeared.
Cancellation is persisted before any in-memory intents are removed, so a
failed write leaves them available to retry.

The ledger notices atomic replacements made by another process, and all
writers share a lock beside it. Cancelling an instance cannot be overwritten
by a later writer using an older in-memory snapshot; dispatch rechecks the
keys under that lock before launching. A TUI already running code from before
this support was installed must be restarted before resuming automations.
Dispatch also refuses to launch when its ledger write fails, keeps the intent
pending, and reports the persistence error in attention.

- `A` toggles a global pause of all automations.
- `Ctrl+A` pauses the selected worktree — or its whole stack when it's a stack member. A stack pause is stored both under the stack's id (the root branch — covers members stacked on later) and as per-member flags (covers the survivors when the root lands and the stack re-roots under a new id).
- `Ctrl+A` in the `h` history pauses an **archived** row, and a pause set while the worktree was live survives into that history. This is not a nicety: the reap drops the per-slug record along with the checkout, while a post-merge `external` run is exempted from its row's death on purpose (see below), so the slugs whose automations can still fire are exactly the ones with no live record left to carry a pause. The flag lives on the removed-history entry, `pausedSlugs` reads it alongside the live ones, and the archived details pane shows it — otherwise "this row is done" and "this row still has a run coming" look identical.

One identity caveat: the circuit breaker and cooldown for `stack.parent_merged` are keyed by the stack id, which changes when the root lands and is cleaned — their accumulated state starts fresh for the re-rooted stack.

Both persist across restarts. Paused rules still evaluate (so state stays current); they just don't dispatch.

There is also a process-level kill switch: `WT_AUTOMATIONS=off` in the environment pins the engine paused for that wt instance regardless of the persisted flags. It exists for probe instances (the TUI test harness, `scripts/tui-test.sh`) running alongside your live one — two engines over the same ledger could double-dispatch.
