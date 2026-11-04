# Automations

`[[automations]]` bindings run an action (or a built-in flow) automatically when a condition holds for a worktree — CI failed, a review requested changes, a stack parent merged. Nothing is automated by default; the section is strictly opt-in. Config fields and examples: [configuration.md](configuration.md#automations--optional-strictly-opt-in).

This page explains the runtime semantics — what "fires", when, and why it won't fire twice. The native implementation keeps pure condition evaluation and the durable fire ledger in `wt-automations`, tracked action execution in `wt-actions`, and source preparation in `wt-sources` and dispatch in `wt-ops`.

The native evaluator is deliberately I/O-free. Its caller supplies one source snapshot, including the per-session GitHub freshness bit, lifecycle clean-candidate evidence, local-day bucket, current pause flags, branch watermarks, and frozen template values. The durable ledger then serializes dispatch claims with a sibling file lock and atomic replacement while retaining fields it does not understand. A dispatched durable prompt whose send reply is lost becomes `ambiguous` and is never blindly replayed.

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
| `wt.merged` | a non-stacked worktree's branch landed (merged / upstream gone / PR merged — the same set the `c` clean sweep uses). A branch with no commits of its own never counts as landed, however contained it looks — see the landed-work guard in the lifecycle service. Rules running `builtin:close-issue`, `builtin:delete-branch`, or a shell action declared `external = true` also evaluate stack members — those runs never touch the worktree, so the clean-vs-restack race the exclusion protects against doesn't apply. An `external = true` shell action also fires for a worktree that is retained rather than cleanable (an owed `--verify-in-*` check, local changes) once fresh GitHub data shows its PR merged: a tracker update is about the landing, not the checkout |
| `stack.parent_merged` | a stack (worktrees chained by their recorded fork bases — see [stacked-prs.md](stacked-prs.md)) has a merged member with open members stacked on it |
| `status.verification_due` | an owed [post-merge check](cli.md#the-check-that-can-only-run-once-it-ships) has come due: a `--verify-in-staging` check once the branch lands on base, a `--verify-in-production` check once the landing reaches `[branch] production` (proved from Git: HEAD, or the exact merge commit of the PR merged at HEAD, is contained in the production branch; without a separate production branch, the base landing). Fires **once per obligation**: the key is the worktree, the stage and a hash of the steps, not the assertion time, so the run's own `needs-human` or `needs-testing` re-assertion after a failure does not rerun it; new steps are a new check. Re-owing identical steps on the same worktree does not fire again while the ledger remembers the key (30 days); after that an obligation still owed fires once more. Settle defaults to **600 seconds**, the delay before the run starts: deploys lag merges, and a run that starts after the deploy has likely finished confirms it once instead of polling for it. Set `settle_seconds` to tune it. The PR-merged leg of "landed" is gated on GitHub freshness. Pair it with a session action whose prompt confirms the deploy for the stage before running the steps |
| `status.verification_overdue` | the branch has LANDED, still owes a [`--verify-in-*`](cli.md#the-check-that-can-only-run-once-it-ships) check that has come due (a production check waits for its promotion, as above), and has owed it for longer than the rule's `after_days` (default 2). **The one trigger here that fires repeatedly**: its fire key carries the local day, so it re-fires once a day until someone asserts `verified`. That is deliberate and is not the echo problem the `by` stamp exists for — a daily reminder is only noise when the recipient can clear it by writing something, and nothing clears this except the check actually happening. Mostly local (state database + merged/gone), but the PR-merged leg of "landed" is gated on GitHub freshness so a boot-stale cache cannot nag about a merge it has not confirmed. Settle defaults to 0: the condition is days old by the time it holds, so a window measured in minutes would only delay a reminder that is already late |
| `branch.advanced` | a watched branch's tip MOVED. **Fleet-level: it targets no worktree**, which is the whole point — by the time a release branch advances, the worktrees whose work is in that range have been swept, so there is no row to key a per-row condition against. Requires `branch = "<name>"`, which must be `[branch] base` or in `[branch] keep_fresh` (the loader refuses otherwise: nothing else advances a local head, so watching one wt never fetches is a rule that can never fire, and a rule that never fires is indistinguishable from a working one). The run launches in the **main clone** with `{{branch}}`, `{{from}}` and `{{to}}`; walking the range is the run's job. Its action's `requires` are judged against no worktree, so a row requirement such as `pr` or `issue.tracker` can never hold there and skips the fire. **First sight fires nothing** — with no recorded tip there is no range, and reading an absent watermark as "everything up to here" would fire once across the branch's entire history. wt records the tip and waits for the next move. The watermark advances only on DISPATCH, so a declined launch keeps its range instead of dropping it. The per-worktree breaker does not apply to this fleet trigger: each tip is a distinct event, and no worktree condition can clear its breaker |
| `mcp.event` | an [MCP Events](https://github.com/modelcontextprotocol/modelcontextprotocol/pull/3415) (SEP-3415, draft) event arrived from a remote MCP server: one fire per event. **Edge-triggered, unlike every other trigger**, so it runs as described in [MCP events](#mcp-events) below. Settle defaults to 0 |
| `status.needs_human` / `status.needs_testing` / `status.ready` | the worktree's asserted [work status](cli.md#wt-status-slug-state--m-note---risk-r) is that state. Local (state database), so no GitHub-freshness gate; the fire key carries the assertion timestamp, so one assertion fires once and re-asserting fires again — unless the re-assert is identical (same state/note/risk/sha), in which case the state write keeps the original assertion timestamp, so it doesn't refire, or the asserter is the session the rule would brief (below). Hyphenated spellings (`status.needs-human`) are accepted aliases. **`status.ready` does not fire while the record carries a [`--blocked-on` gate](cli.md#wt-status-slug-state--m-note---risk-r---blocked-on-gate)**: every documented use of it says "this is yours to merge", and firing that at a branch that must not be merged is the misread the gate exists to prevent, amplified to a macOS banner. The fire is not lost — `--unblock` amends in place, so `at` is unchanged and the fire key was never consumed. Settle defaults to 0 — an assertion is a deliberate write, not flappy derived state |

PR-driven conditions additionally require a **live GitHub fetch this session** — data restored from the persisted cache never fires a rule — and a known `pr.headRefOid` to key the fire against.

## A briefing never echoes its own audience

A `status.*` rule whose run is a prompt action aimed at a live session (`target = "manager"` or `"session"`) does not fire when the work-status record says that same session asserted it. Everything else is unaffected: `builtin:notify` talks to the human, `builtin:clean` and `builtin:restack` talk to git, and a `headless` run is a fresh conversation that wrote nothing, so none of them has an audience to echo back at.

This exists because the manager's *last* triage step is sharpening the `needs-human` note it was briefed about. That re-asserts the state, which mints a new timestamp, which mints a new fire key, which briefs the manager again — quoting its own words back and asking it to triage them. Historical observation: it looped three times for one slug, and the honest answer to the third was "nothing changed".

The wasted turn is not the cost. A briefing whose correct answer is usually "nothing changed" stops getting read, and that spends the one channel that exists for "a worktree is blocked on the human". Note that only `status.*` triggers can loop this way — every other condition is derived from git or GitHub, which no session can write by asserting.

The guard is deliberately narrow in the other direction too: an **unattributed** record (a `wt status` typed in a plain shell, or any record written before `by` existed) is not the audience, so it still fires. A status set with the TUI's `u` picker is stamped `by = "human"`: it still briefs a manager or session, but `builtin:notify` skips it, because the banner would only tell the human what they just did. The attention feed likewise leaves it, and the user's own section moves (including the rows a deleted section returns to the Inbox), to the toast. A guard keyed on stored state has to decide what a missing value means, and here "unknown" must mean "brief it" — the alternative silently swallows real escalations on exactly the rows that have no stamp.

## MCP events

`on = "mcp.event"` turns events from a remote MCP server into ordinary fires. The rule names a `[mcp_servers]` entry (`server`), an event type (`event`, such as `comment.created`) and optional subscription `arguments`; see [configuration.md](configuration.md#mcp_servers--remote-mcp-servers-for-mcpevent).

```toml
[mcp_servers.artifacts]
url           = "https://cozee.artifacts.boo/mcp"
token_command = ["security", "find-generic-password", "-s", "wt-artifacts-token", "-w"]

[[actions]]
id     = "answer-guide"
name   = "Answer a change-guide reader"
prompt = "Someone answered your change guide. Read the whole thread or answer with the artifacts tools, then reply or fix the guide."
target = "session"

[[automations]]
id     = "guide-comments"
on     = "mcp.event"
server = "artifacts"
event  = "comment.created"
run    = "answer-guide"

[[automations]]
id     = "guide-answers"
on     = "mcp.event"
server = "artifacts"
event  = "artifact.answered"
run    = "answer-guide"
```

- **Poll delivery only.** Each rule runs its own `events/poll` loop while a wt TUI runs, at the server's `nextPollMs` (at least 1 second, at most 15 minutes; 60 seconds when the server suggests nothing), draining immediately while `hasMore` is set. Webhook and push delivery need an address the server can reach, which a laptop behind NAT does not have. wt calls `server/discover` first and sends no `events/*` request to a server that does not advertise the extension.
- **The cursor is durable and first sight fires nothing.** Each rule's cursor lives in the state database under `mcpEvents.<rule id>`, next to the events not yet dispatched, and both are written in one mutation per poll. With no cursor, wt polls with `cursor: null`, which the spec defines as "start from now": it records the returned cursor and fires nothing, like `branch.advanced`. Changing the rule's `server`, `event` or `arguments` starts a new subscription from now instead of resuming the old cursor.
- **Each event fires once.** The fire key is the rule id and the server's `eventId`. A restart replays from the stored cursor, a pending event is kept until its fire starts, and a replayed event whose key is already in the ledger never fires again. An event stays pending while its target is busy or paused, and while all automations are paused (`A`): a pause never cancels an event the way it cancels other fires, because the cursor has already moved past it. Events for one target dispatch one at a time, each with its own fire. One that never dispatches leaves after 7 days, a rule holds at most 200, and either discard is reported on the attention feed. Strings in a payload are cut to 2,000 characters and a reply over 1 MB is refused.
- **`truncated: true` is a gap, not an event.** The server could not replay from wt's cursor, so events may have been missed. wt reports that on the attention feed, keeps the fresh cursor, and fires nothing for it. Poll failures (an unreachable server, a refused credential) are reported once per distinct error and retried with backoff from 30 seconds to 15 minutes. An event without an `eventId` or `name` is skipped and reported, never allowed to stall the rule. A poll pass drains at most 20 `hasMore` pages, and stops early when the cursor does not move. A server that returns no cursor on first sight cannot replay, so poll delivery cannot work for that event type and wt reports it; a missing cursor later keeps the stored one.
- **Routing.** An event whose `url` names the page a live worktree recorded with [`wt guide <url>`](cli.md), or whose `artifactId` is the last path segment of a recorded guide on the rule's server host, fires for that worktree: its `target = "session"` action reaches that worktree's session, after the usual quiescence and settle checks. Any other event fires for the fleet and goes to the manager, also when the action names `target = "session"`. An owned event waits while its worktree is paused or archived; it never falls back to the manager. A page two worktrees recorded has no owner and goes to the manager. Only the controller polls: a managed worker never does, so a guide recorded in a worktree on a remote host has no owner here and its events go to the manager.
- **Payloads are untrusted.** Only prompt actions (`kind = "claude"`) with `target = "session"` or `"manager"`, and `builtin:notify`, may run on `mcp.event`. The loader refuses a shell action, which would run people's text as a command, and a `headless` one, whose agent approves its own tool calls with nobody watching; a `slot` is not the event's audience. The fence below helps the receiving agent read the text as data, but it is not a security boundary: anyone who can comment on a watched artifact can put words in front of that session. The prompt gets `{{event}}`, a block that names the event and fences its JSON in `<untrusted-event>` as data, never instructions (`<` and `>` inside it are escaped, so the text cannot close the fence). A prompt without `{{event}}` has the block appended. `{{event_name}}`, `{{event_id}}`, `{{event_server}}` and `{{event_url}}` are also set.
- **It never echoes its audience.** The server is responsible for leaving out what the subscriber itself wrote (artifacts.boo leaves out every agent's comments and answers), so a session's own reply does not wake it again. Because of that, and because each event is a distinct occurrence, the per-worktree breaker does not apply; `cooldown_minutes` still does.
- **Credentials.** `token_command` prints the bearer token, typically from the macOS Keychain. Its output is never logged, stored or shown in an error, and it is fetched again once when the server answers 401. wt has no credential store of its own and cannot borrow a harness's or gateway's MCP login, so the token is one the server issues for headless clients (an artifacts.boo agent token from the workspace's settings).

## Built-in runs

`run` names either an `[[actions]]` id or a built-in flow:

Builtins do not require matching `[[actions]]` entries. Their executors enforce
their own safety checks; configured actions still evaluate their declared
requirements. A local lifecycle inspection failure makes only that worktree's
evidence unknown and reports it in attention. It does not prevent unrelated
worktrees from running eligible automations. Failed local evidence is retried
after 30 seconds even if the input snapshots have not changed, with repeated
identical warnings suppressed. Incomplete GitHub batches still
fail closed for the whole batch.

- `builtin:restack` — the squash-safe stack replay (requires `on = "stack.parent_merged"`).
- `builtin:clean` — destroy one landed worktree (the `c` sweep for a single row). Inherits the sweep's refusal to force: a worktree holding uncommitted changes or unpushed commits is kept and narrated on the attention feed instead of destroyed. That guard matters most here, where nothing prompts and nobody is watching — "the branch landed" is a fact about the branch, and this deletes a directory.
- `builtin:notify` — a desktop notification titled `wt · <slug>` carrying the trigger detail (for status triggers: state, risk, note). macOS uses `osascript`; Linux uses `notify-send`; other platforms record that notification was skipped. Notifications never touch the worktree, so they bypass the quiescence gate — a `status.needs_human` fire happens exactly while the session is busy asking. Pairs with any trigger; the canonical use:

```toml
[[automations]]
id  = "ping-needs-human"
on  = "status.needs_human"
run = "builtin:notify"
```

- `builtin:close-issue` — close the worktree's attached GitHub issue as completed once its branch lands (requires `on = "wt.merged"`; merges via GitHub and via wt look identical to the level condition; one rule max — a second would just race the first). The issue is the `--gh <n>` secondary id, or a `GH-<n>` primary slug id on repos without a tracker; a merged worktree with neither simply never fires. This is the deliberate replacement for GitHub's PR-body `Closes #N` keywords on repos whose feature PRs merge into a non-default branch (e.g. `staging`), where those keywords never fire — and where they do fire, losing the race is fine: failures ("already closed", anything else) are logged as advisory and never retried. A successful close narrates on the ATTENTION feed rather than the firehose (and toasts): it is the one builtin that writes to a system outside wt, where wt's undo doesn't reach and where the board shows nothing. Four deviations from the standard pipeline, all following from "it never touches the worktree and a merge can't un-happen": it also fires for stack members (see the trigger table), it bypasses quiescence entirely, it's breaker-exempt like `builtin:notify` (closing an issue doesn't clear the merged condition — only cleanup does), and its queued intent survives the row's death — the issue number is frozen into the fire at evaluation, so a clean that razes the worktree before delivery (a restack pre-clean, or a manual `c` inside the settle window) doesn't lose the close. Detaching the issue while the row is still alive, or pausing the worktree, still cancels a queued close; a merge that happens entirely while wt isn't running and whose worktree is also cleaned before the next launch is the one case nothing can observe. Each landing closes its issue at most once: the fire is keyed by the worktree's branch as well as its PR, so a landing first seen through trunk containment and later through the merged PR (or the reverse, as when the PR drops out of the snapshot) does not close an issue a human reopened in between. A follow-up landed through `wt next` is on a new branch and closes the issue again. To keep an open issue across a partial landing, detach it with `wt issue <slug> --clear-gh` before the branch lands (this also cancels a queued close); a `GH-<n>` slug cannot be detached, so pause that worktree's automations instead (`Ctrl+A`). Both are per worktree; nothing global is paused.

```toml
[[automations]]
id  = "close-issue-on-merge"
on  = "wt.merged"
run = "builtin:close-issue"
```

- `builtin:delete-branch` — delete the branch's ref on the origin repo once it lands (requires `on = "wt.merged"`; one rule max — a second would race the first to a DELETE that can only succeed once). This is GitHub's **"Automatically delete head branches"** setting, for repos that have not enabled it, and it exists for the same reason `builtin:close-issue` does: the repo-level feature is not always available to you, and the landing is a signal wt already computes. The delete is a `git push --force-with-lease` from the main clone through `origin`, leased on the merged PR's head commit (or, without a PR, the local tip Git proved landed): if commits were pushed after the merge, or the head is unknown, the branch is kept and a "NOT deleting" line goes to the attention feed.

  **It waits while anything is still based on the branch.** GitHub *closes* every open PR whose base branch is deleted by push or API (timeline event `base_ref_deleted`); it retargets them only under its own "Automatically delete head branches" setting, and a PR closed this way cannot be reopened or retargeted. So before deleting, the run lists open PRs with `--base <branch>` and the live worktrees whose recorded fork base is the branch. If either exists, or the `gh` query fails (a failed read is not evidence of absence), it **defers**: nothing is written, and the same frozen fire is requeued and checked again after 30 seconds, every 5 minutes once it is 10 minutes old. A deferral survives the row's cleanup, is not counted by the breaker (nothing was dispatched), and shows in the queue as waiting for "a re-check: <reason>". When a fire has been deferred for an hour, one warning goes to the attention feed naming what still blocks it; it keeps checking after that. In the usual case the `stack.parent_merged` restack retargets the children to the trunk within a minute or two and the next check deletes the branch. A deferral is recorded in the automations ledger (`deferred`, with the fire and the merged tip it proved), so it survives restarts (the next wt to start requeues it) and is not cancelled by pausing or by clearing the queue (`Ctrl+Shift+A`); a pause only holds it. Without a PR number, the tip proven on the first attempt is carried with the fire, so a retry after cleanup has deleted the local branch can still delete exactly that commit. A record still deferred after 30 days is dropped. A child PR opened in the moment between the check and the delete can still be closed; GitHub offers no conditional delete.

  **It confirms the merge against GitHub before deleting, and declines otherwise.** The fire carries the branch and the PR number it claims merged, frozen at evaluation. Dispatch requires a successful `gh pr list --state all --head <branch> --json number,state` response containing that exact PR in `MERGED` state. If the fire had no PR number (the board had not read that branch when it landed), a successful empty result, or a merged PR whose head is the tip Git proved landed, confirms it; any open PR, or PRs that do not include one merged at that tip, block deletion. A failed request is not evidence of absence, so it defers as above rather than decline. This second check protects an effect wt cannot undo: **GitHub closes any open PR whose head ref disappears, and files no close event explaining it.** The fire key is `{rule}:merged:{slug}:{pr|local}`, scoped to the slug and PR number rather than the branch name, with `{rule}:landed:{slug}:{branch}:{created}` beside it on every merged fire: once the landing key is handled, the same landing seen again through the other sighting (with its PR or as `local`) does not fire, while a second numbered PR from the branch does. So the branch and PR number frozen into the fire and the dispatch-time GitHub check above are what stop a previous landing from authorizing deletion of a replacement branch.

  It is the second builtin that writes OUTSIDE wt, so it inherits close-issue's whole shape and for identical reasons: it fires for stack members too, bypasses quiescence, is breaker-exempt (deleting a branch cannot clear the merged condition — only cleanup does), survives the row's death because the branch name is frozen onto the fire at evaluation, and narrates a success on the ATTENTION feed rather than the firehose. The native evaluator gives these external runs frozen inputs so they can finish after the row disappears; a post-merge run that *does* touch the worktree must not join this path.

### Post-merge runs that leave the repository

The external-run safeguards also apply to configured shell actions. **A `wt.merged` rule running a shell action declared `external = true` gets all four**, because `external` already means exactly the property they follow from: the effect leaves the repository, so the run needs nothing from the checkout and must outlive it. Such a fire carries an empty quiescence set, is breaker-exempt, evaluates stack members, and survives its row's death — its template vars (`{{issue_id}}` above all) are frozen at evaluation and it dispatches in the **main clone**, the only checkout guaranteed to still exist. A frozen fire whose `requires` are unmet is dropped rather than kept pending, since by construction nothing about it can change, with one exception: a `pr` or `pr.ready` requirement waits while GitHub has not confirmed the row, because a landing is often seen by `git fetch` before the next GitHub fetch reports its PR. Once the worktree is gone, that requirement holds only if a PR number was frozen into the fire.

This generalization is a bug fix, and the bug is worth keeping in view: the exemption used to be a hand-written list of two run ids, so a config action with all three properties got none of the three exemptions. Every merge queued the tracker transition and every merge dropped it again — `superseded (condition cleared)` — because the `c` sweep archived the row roughly 7 seconds after the landing, inside the 10-second merge settle window. `builtin:delete-branch`, queued in the *same millisecond*, ran fine three seconds later. Two intents born of one event disagreeing about whether their subject still exists is the tell that the exemption is keyed on identity rather than on the property.

Prompt actions are deliberately excluded even when `external`: they are delivered into a session **in the worktree**, so the checkout is precisely what they do need.

  Two things are specific to this one. Failure is usually not failure: a repo with GitHub's own setting on, or anyone deleting by hand, wins the race and GitHub answers `Reference does not exist`, which is the end state we wanted — logged as advisory, never retried. And it refuses `branch.base` outright. No worktree branch is ever the trunk, so that guard is defence in depth rather than a live worry; it is there because this is the one mutation in the codebase whose blast radius is the repo's mainline rather than a retry.

  Once nothing targets it, deleting a merged parent's ref is safe for the stack: wt's restack replays from the `baseSha` anchor in the state database, never from the remote ref. It is also already accounted for locally — the unpushed-commit count treats "no `origin/<branch>` and the branch landed" as a squash merge's pre-squash originals, not as unpushed work, so the destroy guards do not start crying wolf once the ref is gone.

```toml
[[automations]]
id  = "delete-branch-on-merge"
on  = "wt.merged"
run = "builtin:delete-branch"
```

## Dispatch pipeline

When a condition holds and its fire key is unseen, the rule creates an **intent** in an in-memory queue (deliberately not persisted; it rebuilds from conditions on the next boot). Delivery then waits for:

- **Settle window** (`settle_seconds`): the intent must be at least this old AND the worktree free of edits for this long. This is also your window to cancel by just… doing something in the worktree. Merge triggers default to 10s (a merge can't un-happen); `wt.created` and the status triggers default to 0s; everything else defaults to 120s to ride out CI/review churn.
- **Quiescence**: if the worktree has a live session that's working or asking, or an action already running, the `busy` policy decides. `queue` (default) holds the intent until things settle, while `skip` marks the fire handled and drops it. There is deliberately no "force" so automations do not collide with an active or blocked turn, regardless of harness transport.
- **Cooldown** (`cooldown_minutes`): minimum spacing between dispatches per (rule, worktree).

The queue event includes any remaining settle delay. A queued review or CI
remediation normally waits two minutes before delivery; it may wait longer for
an idle session or another dispatch to finish. Pausing and resuming starts a
fresh settle window for conditions that still hold.

An action whose row-backed `requires` are not met is not counted as queued.
For example, a `wt.created` tracker action does not occupy the queue for a
worktree with no tracker issue; attaching one later makes it eligible on the
next evaluation. Frozen post-merge fires retain their separate terminal-skip
behavior because their inputs cannot change.

The title bar shows `auto N queued` (or a paused chip), and
[`wt automations`](cli.md#wt-automations---json) lists each queued fire with
what it is waiting for, or says the engine is not running. The app log records
each fire as it queues (with its first wait reason) and as it leaves the queue,
whether dispatched or dropped because its condition cleared, so a fire that
waited on a busy session until it was no longer needed is visible afterward.

Dispatch goes through the exact same paths keystrokes use (the action launcher, the clean flow, the restack flow) — automations have no special powers.

## Failure handling and the breaker

- A dispatched run that fails is marked delivered and **never retried**. The retry is a new head SHA (push a fix, the condition re-fires under a new key).
- A builtin that proves it wrote nothing may instead **defer**: its claim is released and the same fire requeued with a later re-check (30 s, then 5 min after 10 minutes), keeping its frozen inputs and any facts it proved, so it survives its worktree's removal, restarts and pauses, and never counts toward the breaker. One attention warning is raised if it is still deferred after an hour. Only `builtin:delete-branch` defers today.
- A per-(rule, worktree) **circuit breaker** counts consecutive dispatches after which the condition still held. At 2, the rule trips for that worktree and stops dispatching; it re-arms only when the condition is observed false (i.e. something actually got fixed, by hand or otherwise). Fleet `branch.advanced` fires bypass it because no row can provide that clearing observation. A fire suppressed by a breaker or another terminal pre-dispatch guard is recorded as skipped, so it does not requeue on every pass.

## Watching the queue

`Q` in the TUI opens the automations overlay: what is running now, what is
queued (with when its settle window ends, or what it is waiting for), and the
last ten recorded fires from the ledger (delivered, skipped, cancelled or
ambiguous, newest first). It reads the board, so it updates as the engine
does; `A` pauses or resumes from inside it. `wt automations` prints the same
queue from the CLI.

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
- `Ctrl+A` in the `h` history pauses an **archived** row, and a pause set while the worktree was live survives into that history. This is not a nicety: the reap drops the per-slug record along with the checkout, while a post-merge `external` run is exempted from its row's death on purpose (see below), so the slugs whose automations can still fire are exactly the ones with no live record left to carry a pause. The flag lives on the removed-history entry, the paused-slug set reads it alongside the live ones, and the archived details pane shows it — otherwise "this row is done" and "this row still has a run coming" look identical.

One identity caveat: the circuit breaker and cooldown for `stack.parent_merged` are keyed by the stack id, which changes when the root lands and is cleaned — their accumulated state starts fresh for the re-rooted stack.

Both persist across restarts. Paused rules still evaluate (so state stays current); they just don't dispatch.

There is also a process-level kill switch: `WT_AUTOMATIONS=off` in the environment pins the engine paused for that wt instance regardless of the persisted flags. It exists for probe instances (the TUI test harness, `scripts/tui-test.sh`) running alongside your live one — two engines over the same ledger could double-dispatch.
