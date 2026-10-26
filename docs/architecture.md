# Architecture

wt is a native Rust workspace. The installed `wt` executable is built from
`crates/wt-app`, a shim over the application library in `crates/wt-cli`; running it does not require Bun, Node, a JavaScript bundle, or
a source checkout. The former TypeScript implementation is not included in this
tree; its behavior baseline is commit `d9cd2f4`.

## Workspace map

| Crate | Responsibility |
|---|---|
| `wt-app` | The `wt` binary: passes its release version and build identity to `wt-cli` and nothing else, so a release recompiles only this crate ([ci.md](ci.md)) |
| `wt-app-core` | Foundation of the application: `AppContext`, database, build identity, harness glue, host wire protocol and lane classification, feed helpers. Depends on no other application crate |
| `wt-ops` | Operations both the CLI and the host run: removal and its background worker, creation, fork bases, landing evidence, restacking, the events daemon, updates, managed workers and remote dispatch, plus the command implementations they reuse. Depends on `wt-app-core` |
| `wt-sources` | Everything that feeds the board: the host-local source graph (`sources::start`), its leaves and derived stages. Depends on `wt-app-core` and `wt-ops` |
| `wt-cli` (library crate `wt`) | CLI composition, local and remote host services, TUI source pipeline, application actions, update integration |
| `wt-tui` | Board and modal models, input state machine, terminal rendering and action protocol |
| `wt-config` | Config discovery, merge, validation, resolved process configuration |
| `wt-core` | Pure identity, work-status, section and safety rules shared across domains |
| `wt-store` | Durable SQLite state, repository scoping, payload migrations |
| `wt-runtime` | Scoped task lifetimes, the query layer (sources with provenance, per-source disk cache, confirmed-only safety inputs, optimistic-change engine) and source scheduling |
| `wt-platform` | Bounded subprocess execution, locks and platform services |
| `wt-vcs`, `wt-stack`, `wt-lifecycle` | Git inventory/ref operations, stack replay, and worktree creation/removal safety |
| `wt-github` | GitHub reads and mutations, checks, review requests and merge behavior |
| `wt-harness` | Claude, Codex, OpenCode and Pi discovery, lifecycle, output and messaging adapters |
| `wt-actions`, `wt-automations` | Tracked action execution and typed automation evaluation/ledger |
| `wt-remote` | Native worker protocol and runtime transfer primitives |
| `wt-update`, `wt-launcher` | Verified release installation, activation, boot probe and rollback |
| `wt-skills`, `wt-events`, `wt-dev`, `wt-sst`, `wt-naming` | Skills distribution, webhook daemon, dev supervision, deployment reads and naming domain |
| `wt-tmux` | Cancellable tmux transport for a caller-selected server and socket; never reads wt configuration or ambient `$TMUX` |
| `xtask` | Repository tooling: `cargo xtask gate`, `deps-check`, `hygiene` (see [development.md](development.md)) |

`crates/wt-cli/src/lib.rs` (`wt::main`) is the composition root; the binary
only installs its build identity (`build_info.rs`) and calls it. It resolves one
configuration for the process, opens the repository-scoped database, builds an
`AppContext`, and starts either a CLI operation, the local host, or the remote
worker protocol. `sources.rs` composes the host-local source graph; the GitHub
leaf's fetch scheduling (rate floor, events-daemon arbitration, deferred reads)
lives in `github_source.rs`. A remote
host runs the same source and action pipeline as a local host; the controller
combines the resulting snapshots for display.

Creation emits sparse `wt::creation` stage records with the checkout path,
phase, elapsed milliseconds, outcome, and exit code. These distinguish Rift's
whole create command (including any Rift-owned postcreate hooks), copied-index
refresh, branch checkout, stacked-base fetch, and dependency installation.
An interrupted future records an interrupted phase. Command arguments and
captured output are excluded. A slow Rift command is not by itself proof of a
slow kernel clone; use process sampling during that phase to separate its
internal work from OS filesystem or watcher pressure.

Safety decisions never read display data. TUI cleanup, removal, GitHub
writes, action requirements, restack and automations take
`wt_runtime::Confirmed<T>` values, which only a leaf source can produce from
data this process fetched live (see the query layer below). Cached merged-PR
evidence applies only to its exact HEAD; local hazards and removal revisions
remain authoritative. Git-only landing evidence retains the published-base
check.

The `c` cleanup menu opens directly from the prepared board, including archived
rows. It does not enqueue a command, scan checkouts, or wait for SSH. If no row
is eligible, it shows the keep reasons in a persistent, scrollable overlay.
Opening the menu also requests the ordinary background refresh. The existing
source graph combines repeated requests and publishes updated facts without
delaying the menu. The open confirmation keeps the displayed candidate set;
reopening it uses the latest results.
Confirmation captures only the displayed keys, paths, branches, and HEADs.
Each host then checks the selected rows with its confirmed GitHub evidence and
captures removal revisions for the worker's locked recheck. It does not start
a second PR fetch. Cached merged-PR evidence applies only to its exact HEAD;
local hazards and removal revisions remain authoritative. Git-only landing
evidence retains the published-base check. The prepared board nominates rows;
it cannot authorize removal by itself.

## Source and presentation boundaries

The TUI input loop consumes prepared `Board` snapshots. It does not run Git,
GitHub, harness, tracker, or filesystem scans while handling a key. Each
independent reader is a `wt-runtime` source; actions go through the host
service. `TaskScope` owns source tasks and cancels them together at the host
boundary.

`wt-app::sources::start` builds the local leaves and the derived stages that
project GitHub, action history/log tails, naming, sessions, removed history,
automation status, and optional diagnostics into the board. Fetches are batched
by domain, never issued once per rendered row. Watchers and activity sources
publish only meaningful changes; timers are backstops for inputs without a
reliable local event. Optional source failures are shown beside the last good
data instead of erasing it.

GitHub snapshots also carry the exact branch set queried by that read.
`CoveredPr` distinguishes an unqueried branch, a confirmed absent PR, and a
present PR. Action requirements preserve that distinction in
`ActionPrObservation`; restored board data cannot prove absence or authorize a
write. PR-dependent actions wait briefly for the existing shared leaf to
confirm their branch instead of launching a second GitHub lookup.

`attention_source` observes the existing prepared GitHub, metadata, issue,
dev-server, and manager-session snapshots and narrates confirmed transitions.
It does not refetch those inputs or perform per-row requests. It seeds each
transition tracker quietly, retains its last good baseline across failures,
and bounds the in-memory event tail. Manager report lines remain owned by the
separate activity source. The GitHub comment observer fetches the authenticated
viewer once on demand and never treats comments as foreign while that identity
is unknown.

The history source prepares the bounded SQLite removal ledger when metadata or
inventory changes, even while the pane is closed. Opening history displays
those prepared rows immediately and enriches tracker and landing facts in the
background. Closing history cancels external enrichment but retains its rows.

The activity source backfills a bounded tail from the seven rolling native
JSON logs and manager reports. Attention transitions are written to that same
log with their event timestamp, so both feeds can recover them after restart
without a second durable event database. A one-second metadata backstop checks
only the latest seven log files and manager reports, refreshing the activity
source only when a file changes. Active Codex rollouts have a separate 500 ms
metadata backstop over their exact retained paths. Both keep file I/O off the
input path, detect same-second appends without hashing transcript contents,
and remain responsive when native filesystem notifications are lost.
Rollout UUID lookup caches directory membership against directory identity and
nanosecond modification/change times, including negative searches. An unchanged
history directory does not require re-enumerating or statting all its files.
New date directories and new files invalidate membership immediately; candidate
files still require current UUID, checkout and slot-ownership validation.
Activity cursors are keyed by checkout, slot and thread UUID; reading another
history cannot reset a slot's cursor and feed discovery back into itself.
JSONL reads advance in bounded chunks and retain at most 1 MiB per unfinished
record; oversized records are skipped through their terminator so later events
can still be consumed. An incomplete record does not publish progress until a
complete line is consumed. A live Codex/OpenCode tmux slot without a proven
thread UUID remains busy for lifecycle safety, but no historical transcript is
treated as live.
The session pane shows that identity uncertainty as `unknown`.
The attention watermark is stored in
the repository-scoped state and projected into every board snapshot.

`wt-tui::model` owns cursor, pane, modal, and key dispatch state. The terminal
driver polls input and prepared snapshots and renders them with Ratatui. Actions
cross a typed `UiAction` boundary into the application controller. This keeps
terminal rendering separate from I/O and lets the same host service handle
local and SSH-routed requests.

Menus are presentation of the board, not host requests. `UiAction::menu`
(`wt-tui/src/menus`) declares for every action, without a wildcard arm,
whether its menu is built from the presented board, needs host-only data
(with the reason), or is not a menu. Board builders are pure functions of
`Board` and answer any board, including an empty one; a builder may defer to
the host when its board lacks the data (an unprefetched palette). The host service refuses any action whose menu is board-built. An
open board menu rebuilds on each presented snapshot and keeps its selection
by option value, so option values identify choices, never list positions.

The `d` menu opens from cached board facts for local and remote rows, including
hazardous or deployed checkouts. It shows cached warnings without authorizing
force. Confirmation asks the host to check the displayed path, branch, and HEAD,
then plan removal. Any current hazards or stage destruction need a further
confirmation of the fresh plan. If HEAD is unavailable, a local information
menu opens without a removal action. No host scan runs before the menu opens.

Accepted removals have repository-scoped SQLite records keyed by checkout and
job ID. The metadata source reconciles interrupted result publication from the
durable worker journal. Inventory projects active and failed records into the
Removing section, including records whose checkout is absent. Only the current
job can publish its result. A failed attempt remains until a reviewed retry
succeeds, or, once its checkout is absent from inventory and disk, until a
dismissal witnessing its job, path and branch deletes only that record; no
retry or dismissal can replace an active attempt. Cleanup that fails after the
commit point, such as an undeletable quarantine, is a warning on a successful
removal, not a failed record. Worker liveness checks and
SQLite reads stay in the metadata source, off the terminal input path.

Commits from a menu carry a witness of what it showed: `Seen<T>`, `PrPin`,
`LiveSessionIdentity`, removal identity, or ledger timestamps. The owning host
compares the witness with current state and refuses a mismatch with
`controller::stale`, which every host (local or SSH worker) maps to
`UiReply::stale`. Nothing is written; the model reopens the same menu from its
newer board and asks sources to refresh. Restored or cached board data can
therefore drive what a menu offers, never what a write assumes. A commit with
no witness (CLI, automation) asserts nothing about a view and keeps the
action's own validation.

An open picker whose cursor the user has not moved follows the builder's
newest default when it rebuilds; a moved cursor follows its option value and chord, since two options can share a value.

The action palette is prefetched. While the selection rests (200 ms), the model
sends a background `WarmPalette` for the selected row's and manager surfaces;
the host computes the palette exactly as `PrepareActions` would and publishes
it through the `palettes` source (`Persistence::Never`: it reflects running
actions). Only confirmed-GitHub pickers are published; an active run or
unconfirmed data withdraws the entry, so `!` asks the host. An open prefetched
palette warms again while open. Commits re-run availability on the host; an
action that became unavailable or started meanwhile is refused as stale.

Host-built menus hold keys typed during preparation and replay them into the
menu when it opens, stopping if it closes. Pointer-synthesized keys are not
typeahead.
The header's refresh wave counts prepared refreshing lanes and has a 10 Hz timer
only while moving. Animation copies the last rendered screen and repaints its
header, without rebuilding pane models or requesting source work. Resize falls
back to a full render; modals and mouse selection suspend the animation timer.
The output pane switches between attention, all app activity, and prepared
session/action tails. Mouse-wheel events scroll the pane beneath the pointer;
the terminal restores normal mouse handling when the UI suspends or exits.
Worktree session keys use private tmux client exit codes for direct shell,
diff and agent navigation. The controller keeps the board suspended while the
owning host prepares the next session through the normal session path. SSH
workers perform this loop locally. The agent attachment is retained so a return
from the shell keeps the selected named conversation. A same-session key ends
the attachment and returns to the board.
Interactive tmux panes retain nonzero command exits so startup failures remain
inspectable. Enter retries only a dead pane and clears its stale harness identity;
successful exits close normally. A pane-owned supervisor resets retention before
each foreground command, records its exit status, and enables pane-local retention
only for failures. This avoids older tmux/libutempter exit-status races; background
action and development-server sessions do not use the supervisor.
Session creation streams a quoted tmux command through stdin rather than the
bounded client command frame, preserving long pinned environments and retained
retry commands. Managed and harness PATH composition retains each component's
first occurrence, so repeated sourcing does not grow the environment.
Codex identity
recovery may inspect its sole direct child only after validating the supervisor
marker, then rechecks the child's PID, start time and executable with the pane.
When no writer lock proves it, as with daemon-backed Codex TUIs, wt stamps the
sole TUI rollout of that checkout and slot created since the tmux session
started, only if the pane runs a Codex TUI and no other Codex TUI outside
stamped slots has that checkout as its working directory or argument. Any
other outcome leaves the slot unidentified and nothing is sent.
The session
source batches pane inventory with session inventory, reports new failed panes to
attention, and excludes dead panes from live-agent observations.
Read-only tmux probes can race the server exiting after its last pane closes.
For the exact `server exited unexpectedly` response, they make at most three
observations within the original timeout budget. A later response must prove
presence or absence; persistent uncertainty remains an error. Mutating commands
are never replayed by this mechanism.

Session pickers, the harness chooser and terminal stop are board menus. The
session source prepares `Board.session_menus` and `Board.terminals` for each
local worktree and slot from the background session-history snapshot and the
one tmux inventory the board already holds. Menus open from that board without
a host request and rebuild as discovery completes. Opening `;` also sends a
background `DiscoverSessions` read that queues a history refresh, so saved
conversations join the open picker. Cached history only nominates choices:
attachment and stop check the live tmux identity again, and a mismatch replies
stale so the menu reopens. Remote rows and targets without prepared entries
are answered by their host from the same snapshot. A `New` choice carries the
live slot ids the picker showed; the host refuses it as stale when a slot it
did not show is live for that harness. Terminal stop is local only for a
running local terminal; retained dead panes and remote rows are read by their
host. Target resolution uses
worktree inventory without Git status scans across the fleet.

## Query layer

All board data and all optimistic display go through one layer in
`wt-runtime`. Its guarantees are enforced by types, a lint and conformance
tests rather than by convention.

**Sources and provenance.** A *leaf* source observes one `Domain` (Git,
metadata, GitHub, reviews, issues, sessions, and so on) and is built with a
required `SourceSpec { name, domain, persistence, policy }`. Only a leaf
publisher can mark data live, and every live observation carries a `Stamp`:
the moment its fetch *started*, on the host's own process clock. A *derived*
source (`derived_channel`) combines snapshots it read; its snapshot inherits
their per-domain provenance (`derive`, `with_input`), so a board built from any
restored input reads as restored for that domain. `SourceSnapshot` fields are
private, so projections cannot relabel restored data as live. Leaf readers must
also validate their inputs before publishing their own observations; private
snapshot fields alone cannot establish that an external read used current keys.
`clippy.toml` bans raw
`tokio::sync::watch::channel` outside `wt-runtime`; a control channel that is
not board data says why with `#[expect(clippy::disallowed_methods)]`.

**Persistence.** Each leaf declares `Persistence::Disk(DiskCache)` or
`Persistence::Never(reason)`. Disk leaves (Git inventory, GitHub PR data,
review requests, issue statuses, dev status, session discoveries, Git
presentation facts, remote host snapshots) restore their last live value at
start, off the input path, as `Provenance::Restored` in the `Refreshing` state,
unless a live publish arrived first. They save live, ready data at most every
10 seconds and once at shutdown within a one-second budget that never fails
shutdown. A fetch shows its lane as refreshing only if it runs longer than 100
ms, so a skipped or quick refresh causes no redraw. Files live under `<cache root>/query/<name>.json` in an envelope
whose scope hashes the repository id, canonical main clone, merged
configuration and build (plus the executable's size and mtime for `dev-`
builds). Remote host display snapshots use `<cache root>/remote-query/` instead:
their scope binds the repository, main clone, selected repository config,
endpoint, host protocol, and explicit display-cache schema. Compatible binary
updates and unrelated local settings do not discard remembered remote rows.
They restore as cached, never as confirmed live evidence. Empty startup frames
update connection state without replacing remembered rows. A mismatched,
corrupt, oversized or week-old file is deleted and ignored. The directory is owner-only, identical data is not rewritten, and
temporary files a crashed writer left behind are removed. Sources that read SQLite, logs or live process state are `Never`:
durable state is read directly and never cached here.

**Confirmed inputs.** `LeafHandle::confirmed()` returns `Confirmed<T>` only for
live data (for GitHub this includes an events-daemon snapshot, which is a fresh
read by the daemon stamped with its own fetch start, not a restored cache); a derived handle has no such method, and `Confirmed` has no public
constructor. Remove, cleanup, GitHub writes, reviewer edits, action
requirements, restack and automation facts take `Confirmed` inputs (or `None`,
which behaves like "still loading" and fails closed). `Confirmed::fresh_within`
replaces ad-hoc age checks; GitHub writes and reviewer edits also refuse
confirmed data older than 10 minutes, while landing evidence for cleanup stays
valid at any age because a merged PR at the checked head never un-merges. An
events-daemon snapshot is read only when it names this repository and
configuration (`repository`), was written by this build, and is under 90
seconds old. A fetch whose key set comes from another source
(GitHub branches, issue ids, events-cache coverage) reads that input through
`confirmed()` and skips while it is restored, so a restored inventory never
produces "live" data. Automation quiescence requires confirmed session data and
treats anything else as busy.
Session discovery receives leaf handles for Git and tmux inventory and requires
`Confirmed` values from both before selecting checkout paths. Restored inputs
cannot advance its target baseline; confirming identical restored rows still
starts the first live discovery pass. A confirmed empty session inventory also
finishes that pass, so an unused restored discovery cache cannot remain pending
forever. Empty passes retain the confirmed inputs' observation stamp instead of
claiming a new external read. Manual refresh and the backstop enter this same
confirmed-input path.

**Optimistic changes.** `wt_runtime::Pending<T, M: Mutation<T>>` is the only
optimistic engine. It never writes, so it cannot retry. An entry is *in flight*
until the host answers, then *accepted* with the host's stamp, or *unconfirmed*
when the outcome is ambiguous. It is shown over confirmed data until a fetch
that started at or after the accept stamp shows the change, the write fails, or
a bounded deadline passes (60 s in flight; 30 s accepted for GitHub, reviews and
issues; 12 s for local domains; 30 s unconfirmed; creation and removal, which can
legitimately take minutes, 10 minutes). A change waits only on domains that a
leaf publishes (`wt_tui::SETTLING_DOMAINS`, checked by the source registry
test). While a write waits, `hold_for_settlement` lets identical refetches carry
their newer stamps through derived stages and remote frames, which otherwise
skip republishing when nothing visible changed. An older or out-of-order
fetch, a restored value or a stamp from another process can never settle it.
An expiry while confirmed data still disagrees shows one notice.

- The TUI model begins a `UiMutation` for every write at keypress. Each
  `UiAction` declares its effect in an exhaustive match
  (`UiAction::effect(&Board)`), resolved against the board the user sees, so
  toggles carry an explicit direction and GitHub writes carry the PR number and
  head SHA they target (`PrPin`). The executing host refreshes the domains the
  effect names and replies with a `ReplyOutcome` (`Accepted(stamp)`,
  `NoChange`, `Deferred`, or `Uncertain`). Controller-owned metadata writes
  (including folding, layout and attention acknowledgement) use the same local
  completion classifier as host services: stamp successful writes and enable
  settlement observations before requesting their confirming reads. Explicit
  outcomes and foreign stamps are never replaced by a controller clock.
- Interactive session entry is a terminal handoff, not a pending board write.
  Shell, diff, harness entry and managed sign-in have no expected board
  postcondition or host write stamp; session sources independently observe any
  sessions they create.
  Connection progress and failures still arrive through normal action replies.
- Creation is a `UiMutation` too: `Pending` projects a selected, non-actionable
  operation row immediately. A read-only host plan pins the canonical branch
  and checkout key; the controller waits for the TUI to bind that identity
  before sending the creation mutation. Inventory can then replace the
  projected row by exact host and key even before the final reply. Neither
  stage guesses identity from a title, issue number or newly observed row.
  Normal navigation remains available and late replies cannot reclaim a
  manually changed selection. F12 is a normal action once the real row is
  ready, never queued against an unfinished checkout. Dependency installation
  uses a durable background action, so
  it does not delay agent entry. When a selected row disappears, cursor fallback
  uses the current visual row identity and section, not a stale position.
  Creation captures that section and an anchor key. After checkout creation,
  the controller records the new layout and sends its own acceptance stamp to
  the TUI before the final creation reply. Layout and remote checkout creation use separate pending mutations,
  so neither host's clock confirms the other write. The `creationAfter` layout
  field applies while the row has no explicit order and, under status sorting,
  no work status. The projection preserves stack units and handles missing or
  cyclic anchors without dropping rows.
  J/K selection follows the pending reorder through unchanged snapshots and
  its confirming layout. Filing through the section picker, archiving, or
  removing a row keeps the cursor on a surviving neighbor in its old section.
- The GitHub action worker keeps a private ledger of its own accepted and
  uncertain writes for its no-op guards (disarm is sent whenever the ledger
  shows the PR armed; enable is refused while an earlier enable is
  unconfirmed). The ledger is never published, so automations and planners see
  only confirmed GitHub data.
- Host-side projections (issue statuses an action run is setting) use the same
  engine and publish `PendingView`s in `Board.pending`.

**Uniform display.** Every pane draws its border through
`wt-tui::render::status::pane`, naming the domains it shows; the title reads
`<host> <domain> cached 3m`, `↻`, `loading…` or `<domain> failed` from the same per-domain
provenance on every host. Cache age is never aggregated into a board-wide age.
Dependent readers publish live empty results once their input is confirmed,
even if no external call is needed; an empty input is not an unconfirmed input.
Projections carry only the dependencies of data they actually display. A failed
or skipped read retains provenance; a successful unchanged read confirms it.
Rows and the details title show pending changes with
one marker (`…` in flight, `↻` accepted, `? … (unconfirmed)`).

## Freshness and refresh

Worktree session glyphs and special-session footer colors use the same live
physical-slot activity rule. They remain visible before a transcript UUID can
be attributed. This display-only presence cannot supply history identity or
authorize messages: those still require the exact discovered session. Unknown
physical activity can be enriched only by a proven live transcript from the
same harness. Restored or disconnected remote snapshots cannot claim a live slot.

Git presentation refreshes retain their previous display and observation stamp
until the replacement is ready. Invalidating a probe never publishes an empty
intermediate result that makes landed work briefly look unmerged. The projection
still rejects facts for a changed checkout, HEAD, base or PR identity; retained
display data cannot authorize a mutation.

Local Git inventory, metadata, edit timestamps, sessions, action logs, and
GitHub observations have separate source lanes. Filesystem watchers invalidate
only the facts affected by the event. A one-second metadata backstop detects
lost Git notifications over the same declared watch paths: immediate checkout
children and recursive Git refs/worktree metadata, excluding generated trees
and symlink traversal. Each scan is capped at 8,192 entries and 200 ms, runs off
the input path, and refreshes only the domains whose file metadata changed.
Native notifications for regular files already observed by that probe are
deduplicated against current size, modification time, inode, device and change
time. Directory and unknown-path events remain conservative invalidations.
The watch list derives from the Git source's inventory, minus checkouts with an
accepted removal record and checkouts that no longer exist; it is re-read when
either input changes. A watcher's missing-path error triggers at most one Git
refresh and watch-list re-read per five seconds; other watcher errors keep
their full refresh.
Incomplete scans retain the 60-second full reconciliation backstop. GitHub reads remain batched and rate
limited (10 s minimum interval measured from fetch start); the optional events
daemon supplies push updates stamped with its own fetch start, with a
configured poll backstop for missed events or daemon downtime. An accepted
GitHub write bypasses the events cache, and while any of the worker's writes is
unconfirmed it keeps requesting direct reads (still rate limited) until one
shows the write or its deadline passes, so a lagging read model does not make
the change expire. A Sessions refresh also reruns the session-activity pass,
which re-reads the persisted primary harness.

`r` requests an ordinary source refresh and keeps durable data and derived
caches. `Ctrl+R` asks for confirmation, clears the derived naming cache and
GitHub picker memory, bypasses a cached webhook snapshot once, and requests
fresh source data. It preserves authoritative SQLite state, running actions,
automation history, and agent-session identity. The performance pane is dormant
while closed; opening it takes a snapshot, and continuous sampling is opt-in.

## Host commands and remote execution

`wt-app::controller` accepts typed UI requests, assigns them to a host lane, and
tracks running commands through shutdown. `HostService` performs the host-local
operation and returns a typed reply. Accepted mutations belong to the command
drain, not the lifetime of a particular SSH view; read sources are cancelled
when their host scope ends.
Attention, history, and performance controls have a separate serialized slot
reserved beyond the four ordinary action slots, so long host operations cannot
delay them. Attention acknowledgement returns its own accept stamp before
refreshing metadata. Read-only preparations, worktree creation and terminal
entry also have reserved capacity. Title, issue and status writes share a bounded
metadata slot independent of ordinary host capacity. They conflict with removal
of their own target, including a fleet cleanup that removes it, but not unrelated
removals or cleanups, creation or long host actions such as compaction and GitHub requests. Transactional field
updates and captured revisions protect independent metadata writes. Palette title actions use the same classification as direct
title actions. Terminal handoffs serialize with each other;
worktree entry also conflicts with removal of that exact host and checkout,
whether single or part of a fleet cleanup. Special-session entry and unrelated checkouts remain
available during background writes. Preparations do not wait behind mutations because the
confirmed write revalidates its inputs. Removals serialize by host and target;
unrelated creates and removals can proceed concurrently, while broader host
operations retain their conflict boundaries. A fleet cleanup is one accepted
operation that owns exactly the `(host, checkout)` pairs it removes: it
serializes with another cleanup and with whole-host operations on a host it
touches, and with checkout-scoped work only on those checkouts, so entry and
writes elsewhere never wait for its teardowns. Waiting notices name the
blocking operation and its checkouts. Lifecycle
per-target locks serialize create/remove races on the same slug.
Interactive actions also use a process-admission pool separate from background
reads and lifecycle jobs. Both pools are bounded and shared for the process
lifetime; cloning a context does not create capacity. The controller's action
classification selects this pool in local and managed-worker execution, including
the repository and tmux adapters. Session identity resolution enumerates checkouts
without scanning their Git status, so unrelated dirty trees do not delay entry.
Action admission declares every action's resource scope in an exhaustive match.
Host preferences and cancellation controls share the reserved control capacity;
they do not acquire checkout or creation locks. Checkout-specific commands
exclude removal of that checkout, not creation elsewhere. A new action must
choose its scope explicitly instead of inheriting whole-host exclusion.
While commands run or wait, the controller sends nonterminal progress after
three seconds, including elapsed time and the conflicting operation or capacity
wait. These notices cannot settle an optimistic mutation or complete a menu
request, and stale notices cannot overwrite feedback for newer input.
Quitting closes command admission and cancels source reads, including automation
evidence preparation. Already-claimed automation dispatches and accepted local
writes retain their command token until their outcomes have been persisted;
after three seconds and every ten seconds their drains report the operation,
target, and elapsed time. Each operation owns its timeout and safe stopping
points, so a UI deadline cannot interrupt a write. A local removal may stop
waiting only after its durable worker record says running and the recorded PID
and process start identity still match. The accepted worker continues
independently; wt reports that status as accepted and points to `wt logs`, never
as a completed removal. After accepted operations finish or detach at that
durable boundary, background source cleanup gets one five-second grace period,
followed by database shutdown. Runtime teardown
gets at most one additional second for residual blocking reads. The source
cleanup grace is not a deadline for accepted writes, and an error does not start
a second source grace period.
Native filesystem watcher setup and teardown run on the blocking pool. Sources
await teardown, including cancellation during setup, so OS thread joins cannot
block the async executor or outlive successful source cleanup.
`wt-app::watcher` selects the backend centrally: macOS uses `PollWatcher` with
metadata-only comparisons at a one-second interval, while other platforms use
notify's recommended native watcher. Do not enable macOS kqueue here: recursive
directory link events can rebuild descendant watches and close unrelated open
SQLite WAL/SHM descriptors. Closing a SHM descriptor releases SQLite's
process-wide POSIX locks and can corrupt a concurrent writer, so keep
`compare_contents(false)` to ensure polling never opens watched files.
Source-specific reconciliation and metadata backstops remain responsible for
recovering missed events.
Known create failures retain the input and selected host for correction. A lost
remote reply remains ambiguous and never opens an automatic retry path.
Both failed action replies and raised errors produce one persistent attention
event. Uncertain writes produce a warning there; no-op notices remain transient.
Intermediate terminal handoffs carry the originating request ID and input
generation. A newer prompt or selection can decline the ticket before terminal
suspension; no attach client starts, the prepared session remains alive, and a
declined sign-in does not execute its continuation. Navigation that declines a
pending connection cancels its controller wait and releases terminal admission.
A current handoff transfers ownership to the terminal before the request ends,
so completing the request does not cancel an active attach client. A late empty completion cannot
erase feedback from newer input, including a worktree still being created.

The SSH protocol is framed and versioned. The controller probes the worker,
checks its role and protocol, and provisions the exact native build before
binding commands to that runtime. Same-platform workers can use the running
verified binary; cross-platform workers require the matching published native
artifact. A protocol/build mismatch is reported rather than silently running a
different command implementation. Configuration is resolved independently on
each host, so each process has one selected config and its own repository state.

A managed remote adds one step between staging the runtime and the handshake.
`wt-app::managed` builds a `wt_remote::managed::ManagedRequest` from the
controller's config layers, git settings, GitHub token, Codex home, and
`[managed.env]`, and the runtime's config-free `_managed apply` command writes
it under the worker's managed root, ensures the clone, and returns typed
checks. The host actor keeps the latest checks in a watch channel; the fleet
projection turns them into attention lines, and `host_dispatch` turns an owed
sign-in into a confirmation before the request that needs it. The worker
sources the generated `env.sh` before every command. `WT_SESSION_ENV` names
the variables that agent tmux sessions inherit, and each session command
sources `secrets/env.sh` itself (`WT_SESSION_SOURCE`) so secret values never
appear in a process's arguments.
Controller-owned hook dependencies are declared in `[managed.files]` and mirrored
under `WT_MANAGED_FILES`. Missing or unreadable declared inputs fail provisioning
before sending an update. Their isolated tree preserves literal bytes and repairs
worker drift; it cannot overwrite generated configuration or credentials.

Explicit `[managed.services]` entries expose fixed controller executable argv
through `wt controller serve`. This service runs independently of the TUI and
owns private Unix sockets forwarded to managed workers over SSH. A persistent
controller identity separates socket names across machines. Every tunnel must
pass a round-trip probe before it is reported ready. Before binding a reverse
forward, the worker reclaims only an owned, unchanged stale Unix socket whose
connection probe is refused. Live, newly created, replaced or uncertain endpoints
are retained. This recovery runs under an endpoint lock and does not depend on
global SSH server settings. Requests name a configured
service, never an executable or working directory; service processes run in the
controller's main clone. Admission, input, output, concurrency and execution
time are bounded. Accepted calls drain on shutdown, and transport failures never
replay them. Application adapters own operation IDs, reconciliation and file
transfer. No Keychain or OAuth refresh-token export is involved.

Codex account changes record a credential-free generation under the controller's
selected Codex home. Provisioning and the controller service propagate it to
workers, which retain pending intent across disconnects. Only native login,
login-status verification and daemon recovery acknowledge that generation.
Terminal preparation rechecks stale or pending sign-in reports before entry.
Existing tmux sessions and unrelated account homes remain owned by their users.

## Linux workload resource limits

`wt-config` owns the optional `resources.linux` policy and validates every
limit before app composition. `wt-app::workload` converts the selected policy
into explicit helper arguments. Managed-worker generation merges each host's
resource override into its worker config. An unmanaged worker owns its own
configuration.

Agent, shell, and diff tmux clients receive a command prefix. Action workers
and dev supervisors apply the same prefix to their job commands, retaining
their durable completion and restart control outside the resource group.
Terminal prefixes run inside the pane and inside the retained-command
supervisor, so an existing tmux server cannot bypass the job boundary. The
config-free `_workload` helper uses `wt-platform::resources` to configure the
shared systemd user slice, place its own PID in a transient scope, and check
its membership and effective cgroup CPU/memory files. It then executes the
original argv in the same process. Configuration is not loaded a second time
in this helper. A failed setup never executes the workload.

A host-local file lock serializes shared slice changes and scope entry for
one resource group. Shared settings are runtime settings; user and repository
configuration remain authoritative. The board, host service, and tmux server
are not moved. Separate service daemons require their own resource policy.

## Durable state, caches, and operation safety

`wt-store` is the authority for repository-scoped status, sections, issue
overrides, fork bases, archives, removed-worktree history, automation pause
state, and other user-authored records. SQLite schema migrations and the
versioned worktree-state payload migrations are distinct compatibility
boundaries. `wt state migrate` imports legacy records; it does not run as an
implicit cache clear.

Generated naming summaries, the per-source query cache, event observations,
action log tails, and source snapshots are rebuildable. Hard refresh may clear derived
summaries; it must not delete durable state or resend an accepted operation.
Action metadata and logs remain tracked on disk so a disconnected controller
can re-read completion and output.

Worktree removal and cleanup use the lifecycle service and per-worktree
operation locks. A confirmed removal moves its row to Archived with a
removal glyph at once. Revision capture streams complete Git output and
untracked file contents into fingerprints with bounded memory, cancellation,
and deadlines; evidence is never truncated to satisfy a capture limit.
The worker archives the row before its startup reply and retains that state
on failure. TUI actions await the durable
job's terminal outcome off the input path, so startup does not settle the pending
removal. A later failure keeps it archived and reports an attention event.
CLI background mode returns the startup acknowledgement. Removal errors and completion enter the
attention feed. Cleanup does not force-remove dirty or unpushed work, and a
landed branch with an outstanding verification gate remains protected. Missing
GitHub or inventory facts are unknown, never proof that removal is safe.

Stacks are inferred from each worktree's recorded fork base. Preserve its
`baseSha`: it anchors squash-safe replay. A trunk fork base means the worktree
has no stack parent. “Landed” requires commits beyond that recorded base plus
the relevant Git/GitHub proof; reachability alone is vacuously true for an
unstarted branch.

## External effects and failures

Production process work goes through `wt-platform::process::ProcessRunner` with
an argv vector, working directory, timeout, output bound, and cancellation
token. Avoid shell construction for user values. Long-running child processes
and process groups are reaped by their owning scope. Expected domain errors use
the owning crate's error type; the CLI and TUI render actionable messages at
their boundaries.

GitHub reads are batched and all-or-nothing when a batch is incomplete. A
missing PR result is not an empty-success result. Mutations that can be
ambiguous are not retried unless the service can prove the first operation did
not start. Merge-queue submission and classic auto-merge are distinct paths:
arming follows the PR base branch, while cancellation inspects the PR's actual
armed state. Retries retain the expected head SHA. Async merge submissions use
HTTP 202/409 for pending; a successful HTTP 200 poll may still describe a
pending or failed operation. Only a terminal operation outcome settles the
command; an unknown outcome never authorizes resubmission.

Codex queued message delivery is not idempotent. After a lost response, wt
reconciles the queue and recent thread items. If neither proves whether the
message was accepted, it reports ambiguity and stops instead of risking a
duplicate. The live terminal remains the sole owner of harness questions and
approvals.

Physical checkout deletion uses `ProcessRunner::run_to_completion`: queue
admission remains bounded and cancellable, while an admitted process runs to
completion with bounded output capture. Its caller retains ownership and the
lifecycle lock. A short read-command deadline cannot interrupt Git while it
unlinks checkout files. Other process operations keep their normal policy.

## Release and configuration boundaries

The stable launcher and app use `wt-update`'s immutable, checksum-verified
release artifacts. The candidate is probed before activation; failed startup
restores the last-good version. `wt update` does not fast-forward a source
checkout. Release and rollback state live under the install root, separate from
repository configuration and SQLite data. See [updates.md](updates.md).

`wt-config` resolves `$WT_CONFIG`, XDG/user config, and the nearest repository
`.wt.toml` into one validated value at process startup. A linked worktree may
need discovery through its `.git` pointer and `commondir`; when that resolves to
the main clone, its repository config is the selected file. Runtime config is
not hot-reloaded. See [configuration.md](configuration.md).

## Navigation for contributors

- [CLI reference](cli.md) documents commands, flags, output, and exit behavior.
- [TUI reference](tui.md) documents layout, keybindings, and modal behavior.
- [Backends](backends.md) documents linked worktrees and independent rift
  clones.
- [Automations](automations.md), [stacked PRs](stacked-prs.md),
  [GitHub events](github-events.md), and [manager](manager.md) document their
  domain-specific contracts.
- Run crate tests with `cargo test -p <crate>`; the workspace gate is defined
  by `cargo xtask gate`.
