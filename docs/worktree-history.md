# Worktree history

wt keeps a small durable log of what happened to each worktree, so "how did
this row get here?" has an answer without reading logs: when it went ready,
whether the nudge went out, how many review rounds it took. The TUI shows it
as a timeline (`H`, see [tui.md](tui.md)); `wt history` prints it
([cli.md](cli.md#wt-history-slug---json---limit-n)).

The log is history, not authority. Nothing reads it to decide whether a write
is safe, and clearing a status or a layout never "refreshes" it. It also never
costs a state write: an event that rides on a status, creation or removal
write is appended under a savepoint, and if the append fails (a later build
changed the table under a session still pinned to this one) the state write
lands without it and wt warns once.

## Events

The vocabulary is `WorktreeEvent { at_ms, event }` in
`crates/wt-core/src/worktree_event.rs`. Free text (a note, a message) is kept
as its first line, cut to 160 characters; that bound applies to every stored
string (`by`, `blocked_on`, `base`, `onto`, a rule id). A `checks` event keeps
six failing names and counts the rest in a final `+N more` entry.

| Event | Recorded when | Written by |
|---|---|---|
| `created` | the worktree is created, with its base, dated when creation began | the store's creation write |
| `status` | the asserted state, risk, merge gate or post-merge check changes | the store's status write |
| `status_cleared` | the status is cleared | the store's status write |
| `removed` | the removal is recorded, with whether the branch had landed | the store's removal write |
| `message` | a message is sent to the worktree's agent | `AppHarness::send_recorded` |
| `automation` | a fire finishes or is skipped for good | the automation source |
| `watch` | a watch is set, holds, clears its gate, tells the person, wakes the session, is cancelled or is dropped | the store's watch writes |
| `restack` | a restack replays the branch or stops on a conflict | `restack_action::record_outcome` |
| `pr`, `checks`, `review`, `merge_queue`, `conflict`, `landed` | the fact is observed to change | the host's observer |
| `status`, `status_cleared` (again) | a status was written by a build without the log | the host's observer |

A status event is written in the same transaction as the status, by the one
function every status write passes through (`wt status`, the TUI's `u` menu,
the manager, automations, `wt new -m`). A re-assertion that changes only the
note records nothing. `by` is the asserting worktree's slug or `manager`, and
is absent for a person.

A message's `from` is the sending worktree's slug, `manager`, `wt` for a
prompt wt sent itself (an automation, a restack conflict handoff), and absent
for a person. `delivered` is `true` when the harness proved arrival, `false`
when the send failed, and absent when it was submitted without proof. Messages
to special sessions (the manager, the main checkout) are not recorded: they
have no row. `wt agent start` records the text `start`, not the skill prompt.

A watch's wake-up prompt is not a `message` event: the `watch` event with
`change: "woken"` is its record, and carries `unconfirmed: true` when the
prompt was sent without proof it arrived. The timeline and `wt history` read
a watch's events as two lines, when it was set and what wt did; `--json`
prints every event as stored.

An automation fire that will run again (not started, deferred, still queued)
is not an event. `delivered` means it ran, `failed` that a built-in ran and
reported failure, `unconfirmed` that a prompt was sent without proof, and
`skipped` that the engine ended it without running (the target was busy under
`busy = "skip"`, its requirements stopped holding, or the breaker was open).

A restack records `ok: true` on the branch it was asked for when at least one
worktree was replayed, and `ok: false` on the branch that conflicted. Other
members of the stack get no event: `wt-stack` does not report which branches
it replayed.

## Observed events

Pull request state, checks, reviews, the merge queue, conflicts with the base
and landings change outside wt. The host that owns the board's sources
(`crates/wt-sources/src/worktree_history.rs`) watches them and appends what
changed. CLI invocations never observe.

- **Confirmed inputs only.** A row is observed once the board's Git inventory
  and metadata are live. Pull request facts need a confirmed, ready GitHub
  read that covered the row's branch: a failed fetch that still shows its last
  data, a restored cache and a branch the read did not cover all record
  nothing. Conflict and landing facts need live Git presentation facts.
- **The log is the memory.** What was last seen is not kept in the process.
  Each observation is decided against the slug's own events
  (`wt_app_core::worktree_history::transitions`), and decided again under the
  write lock before anything is inserted (`Store::reconcile_worktree_events`).
  A restarted host, or a second host on the same database, adds nothing for a
  fact that is already there. An observation that changes nothing takes no
  write lock.
- **The first observation is a baseline.** A worktree that predates the log,
  or was just created, gets its current facts once, dated when wt first saw
  them: an open pull request, a finished check verdict, a conflict. Earlier
  transitions are not reconstructed, and a clean, PR-less branch records
  nothing.
- **What counts as a change.** A pull request records `opened` (or
  `opened_draft`), `ready_for_review`, `back_to_draft`, `closed`,
  `reopened`, `merged`. Checks record `failing` with the failing names
  whenever that set changes, and `passing` only as a pull request's first
  verdict or after a failure; running checks are not a verdict. The review
  bot records `findings` when its unresolved count changes and `clean`; a
  verdict on an older head is ignored. People record `approved` and
  `changes_requested`. The merge queue records `queued` and `left`; leaving
  by merging is the `merged` event alone. `left` is recorded only when the
  queue read is known to be complete: the read takes one page of 50 entries
  (`GithubData::merge_queue_complete`), so a pull request missing from a full
  page, or from an events-daemon snapshot, is unknown and records nothing. A conflict records
  `resolved: false`, and `resolved: true` only after one. Verdicts recorded
  for a branch's earlier pull request do not carry over to its next one.

- **Statuses written without an event.** A status write records its own
  event only in a build that has the log. Sessions keep the build that
  started them for days, so an agent's `wt status` from an older build
  changes the state and logs nothing. The observer therefore also compares
  each row's stored status (state, risk, merge gate, owed post-merge check,
  or no status) with the slug's latest `status` or `status_cleared` event and
  records the difference: `from` is the log's last known state, the rest
  comes from the record, and the event is dated at the record's own `at`. It
  never sorts before the slug's `created` or its newest event: it takes the
  later time, and an unreadable `at` reads as now. A `status_cleared` found
  this way has no author and is dated when wt noticed. The status compared is
  the one the store reads in the deciding transaction, not the observer's
  snapshot, so a write that logged its own event, a restart or a second host
  never produces a second one. A worktree with a status and no status event
  gets it once as its baseline; a status older than the 90-day retention is
  left out.

A fact a read does not know keeps its last known value, so GitHub failing or
refreshing and coming back is not a change on every row.

Rows of remote hosts are observed by the remote host, in its own database.

## Storage

Events live in `worktree_events(id, repo_id, slug, at_ms, kind, data)` in the
state database, indexed on `(repo_id, slug, at_ms)`; `data` is the event as
JSON. The table is schema version 2. A build that knows only version 1 checks
for `version >= 1` and never reads the table, so pinned older sessions keep
opening and writing a migrated database. A read-only open of a database that
has no table yet reads as no history.

Events are ordered by when they happened, then by insertion. Where an
incarnation starts is decided by insertion order, not by timestamps: it is
opened by the slug's most recently inserted `created`, and a `removed` or an
older `created` inserted before that one, and anything inserted up to it,
belongs to an earlier incarnation even if a clock says otherwise (the same
millisecond, a clock stepped back, a creation dated from its start). Events
inserted after the previous incarnation closed are placed by time. Every
reader (the board's leaf, `wt history`, the reconcile transaction) uses the
one query that applies this. A slug can be
removed and created again: a slug's visible history starts at its latest
`created` event, and a removed slug reads as its last incarnation. When a
worktree's creation is not in the log (it predates the log, or a pinned older
build created it), the next status or removal write first records `created`
at the stored `createdAt`, unless that is past the retention age or the slug
is already at its event cap. Ordering by
time puts it before whatever was already observed for that incarnation, so
its events do not continue the previous incarnation's history.

A `created` event is dated when creation began, not when it finished. The row
is in the Git inventory while its checkout is prepared and its install runs,
so a status, an automation fire or an observation made in that window sorts
after the creation and belongs to the new incarnation, as does the owed check
a restored worktree carries over. The event remembers the `createdAt` it was
written for, which is how a later write tells this incarnation's creation
from an earlier one's. A creation that fails records no `created`; anything
recorded during it stays on the slug's previous history.

The outcome of a removed row's post-merge check (`wt status verified` on a
removed slug) is recorded only while that removal is still the slug's last
incarnation. If the slug names a new worktree by then, nothing is recorded.

Status writes that replace a whole slug entry are not status assertions and
record nothing: creation's reset and its rollback, the startup reap of dead
slugs, and `wt state migrate`.

Each slug keeps its newest 500 events, and events older than 90 days are
dropped. An append prunes its own slug; the host sweeps slugs that are never
written again once an hour.

## On the board

The `row-history` leaf publishes the newest 40 events of every live local
worktree, and of every removed one the state still lists, in one query, as
`Board::row_history` keyed by row key, oldest first. A removed row's history
is its last incarnation, ending at its removal, under
`RemovedHistoryRow::key`. `wt history` reads more (up to 500).
It reads SQLite directly and is never cached (`Persistence::Never`), waits for
a confirmed Git inventory before reading, and re-reads when the metadata leaf
does: the state database's file watcher and backstop refresh both, so an
append by another process (`wt status` in an agent's shell) reaches the board
the way a status does. A read is skipped when the log has not grown. A read
that fails publishes an empty history and warns: the timeline is blank, and
the removed-rows pane, which shares the History domain, is unaffected.

A remote row's history comes from the remote host's own board and is re-keyed
with the row. The same slug on two hosts is two worktrees and never shares a
history. Event text is stripped of terminal control sequences before it
reaches the board.

The controller prepares `Board::history_times`, a day and clock label in its
own timezone for each history timestamp, off the render thread.
