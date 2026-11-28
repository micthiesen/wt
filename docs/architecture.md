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
| `wt-host` | The host: controller and lanes scheduling, host service and dispatch, action palettes, remote hosts and the fleet board, the terminal palette loop. Depends on `wt-app-core`, `wt-ops` and `wt-sources` |
| `wt-cli` (library crate `wt`) | Process composition (`wt::main`) and the CLI command implementations |
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
| `wt-skills`, `wt-events`, `wt-dev`, `wt-sst` | Skills distribution, webhook daemon, dev supervision and deployment reads |
| `wt-tmux` | Cancellable tmux transport for a caller-selected server and socket; never reads wt configuration or ambient `$TMUX` |
| `xtask` | Repository tooling: `cargo xtask gate`, `deps-check`, `hygiene` (see [development.md](development.md)) |

### Application layering

The application is five library crates in a strict order, each depending only
on those before it: `wt-app-core`, then `wt-ops`, then `wt-sources`, then
`wt-host`, then `wt-cli` (library crate `wt`). `wt-app` is the binary on top.
The order is enforced by Cargo: a lower crate cannot name a higher one. When
code in a lower layer needs something from a higher one, move that item down
to where both can reach it (as `Lane`, `stale` and the feed helpers moved to
`wt-app-core`); never pass it around the boundary with callbacks or
duplicates.

- `wt-app-core`: context, database, build identity, harness glue, the host wire
  protocol, lane classification, feeds and small shared helpers.
- `wt-ops`: operations the CLI and the host both run, and the command
  implementations they reuse (`wt_ops::commands::{_destroy, new, remove, ...}`).
  `wt_ops::maintenance` is the only code that removes leftover files and old
  slug records, and never a lock file; the TUI process runs it daily ([updates.md](updates.md#leftovers-and-the-daily-maintenance-pass)).
- `wt-sources`: the source graph that prepares the board.
- `wt-host`: the controller, host service and dispatch, remote hosts, palettes.
- `wt-cli`: `wt::main` and the remaining CLI commands.

Splitting keeps CI and test rebuilds proportional to the layers an edit touches
(an edit to a CLI command recompiles `wt-cli` alone). It costs release binary
size: each crate keeps its own copies of the generic and inline code it uses,
which stable Rust cannot share in optimized builds, so the five-crate binary is
about 27% larger than the single crate was (38 MB against 30 MB, v0.1.35). See
[ci.md](ci.md) for how caching relies on the split.

Each module logs under its crate's name (`wt_sources::freshness`,
`wt_ops::events`), except explicit targets such as `wt::origin` and `wt::removal`.
The activity feed accepts any `wt_*` target; anything that filters or matches on
a target must use the module's current crate.

`crates/wt-cli/src/lib.rs` (`wt::main`) is the composition root; the binary
only installs its build identity (`build_info.rs`) and calls it. It resolves one
configuration for the process, opens the repository-scoped database, builds an
`AppContext`, and starts either a CLI operation, the local host, or the remote
worker protocol. `wt-sources`' `sources.rs` composes the host-local source
graph; the GitHub leaf's fetch scheduling (rate floor, budget pacing,
events-daemon arbitration, deferred reads) lives in `github_source.rs`. A remote
host runs the same source and action pipeline as a local host; the controller
combines the resulting snapshots for display.

Creation emits sparse `wt::creation` stage records with the checkout path,
phase, elapsed milliseconds, outcome, and exit code. These distinguish Rift's
whole create command (including any Rift-owned postcreate hooks), copied-index
refresh, branch checkout, stacked-base fetch, and dependency installation.
The origin fetch is `fetch-origin`, or `fetch-origin-including-maintenance`
when ref maintenance runs before the checkout (Rift, or a local trunk or
`keep_fresh` base). `origin-branch-probe` is the `ls-remote` after a reused
fetch, `origin-maintenance` is maintenance run inline, and
`dependency-install-queue` is the wait for an install slot. Maintenance
handed to the detached `wt _maintain-origin` worker is not a phase: the
worker logs `ref maintenance after creation finished` under `wt::origin`.
An interrupted future records an interrupted phase. Command arguments and
captured output are excluded. A slow Rift command is not by itself proof of a
slow kernel clone; use process sampling during that phase to separate its
internal work from OS filesystem or watcher pressure.

Safety decisions never read display data. TUI cleanup, removal, GitHub
writes, action requirements, restack and automations take
`wt_runtime::Confirmed<T>` values, which only a leaf source can produce from
data this process fetched live (see the query layer below). Cached merged-PR
evidence applies only to its exact HEAD, or to a HEAD that Git proves holds
only copies of that merged head (`wt_vcs::published_head_holds_everything`);
local hazards and removal revisions remain authoritative. Git-only landing evidence retains the published-base
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
captures removal revisions for the worker's locked recheck. It plans four
rows at a time and starts each removal as soon as that row's plan passes; a
row that fails to plan is kept and the rest go on. It does not start
a second PR fetch. Cached merged-PR evidence applies only to its exact HEAD or to
copies of it; local hazards and removal revisions remain authoritative. Git-only landing
evidence retains the published-base check. The prepared board nominates rows;
it cannot authorize removal by itself.

## Source and presentation boundaries

The TUI input loop consumes prepared `Board` snapshots. It does not run Git,
GitHub, harness, tracker, or filesystem scans while handling a key. Each
independent reader is a `wt-runtime` source; actions go through the host
service. `TaskScope` owns source tasks and cancels them together at the host
boundary.

`wt_sources::sources::start` builds the local leaves and the derived stages that
project GitHub, action history/log tails, titles, sessions, removed history,
automation status, and optional diagnostics into the board. Fetches are batched
by domain, never issued once per rendered row. Watchers and activity sources
publish only meaningful changes; timers are backstops for inputs without a
reliable local event. Optional source failures are shown beside the last good
data instead of erasing it.

GitHub snapshots also carry the exact branch set queried by that read.
`CoveredPr` distinguishes an unqueried branch, a confirmed absent PR, and a
present PR. Action requirements preserve that distinction in
`ActionPrObservation`; restored board data cannot prove absence or authorize a
write. An action palette entry that needs a pull request waits on the board,
in the open palette, for the shared leaf to confirm its branch: up to 12
seconds, cancellable, with no second GitHub lookup. The host waits up to the same
12 seconds before a commit that depends on a pull request (`RunAction` for
such an action, a reviewer edit, mark ready, merge arming, ship, a
failed-check fetch) when its confirmed read is older than 10 minutes or does
not cover the branch. A row's pull request fact reads unknown once the GitHub read
behind it is older than 10 minutes (`PR_EVIDENCE_AGE`), the bound the host
holds its GitHub writes to.

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

Each worktree's own history is a separate durable event log
([worktree-history.md](worktree-history.md)). The `row-history` leaf reads the
newest events of every live local row and every listed removed row from
SQLite in one query whenever the
metadata leaf refreshes, and publishes them as `Board::row_history`. The same
module runs the observer that records pull request, check, review, merge
queue, conflict and landing changes: it takes only confirmed GitHub and Git
facts, and decides what is new from the log itself inside the appending
transaction, so restarts and a second host on one database record a fact
once. Remote rows carry the history their own host published.

The `watch-board` stage puts what each local row waits for on the row
(`BoardRow::watches`: every condition with whether it holds, and what wt does
then), with the worktree's stored creation stamp that the notify menu's commit
carries as its witness. It reads nothing itself: watches are part of the
repository state the metadata leaf already reads, so a watch set in another
process arrives through that leaf's file watcher. While any row waits the
stage prepares the board again every 30 seconds, and exactly when a
waited-for time comes due; a board on which nothing waits is never woken by
time. Remote rows carry what their own host prepared. A watch kept after its
worktree was removed has no row and is not on the board.

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

The driver draws once per batch of input, not once per event: after an input
event it applies every event that has already arrived and then draws the
frame they leave. A frame that costs more than the gap between events (a held
key or a wheel flick on a loaded machine or a slow terminal) is skipped
instead of queued, so the cursor never trails the keyboard, and the last
event of a batch is always drawn. A batch ends early at a key that sends a
request and at a released drag selection. Because events share frames, input
handlers must not read a position only the last frame computed; state a
frame derives (a clamp, a viewport top) has to stay correct across several
events.

Menus are presentation of the board, not host requests. `UiAction::menu`
(`wt-tui/src/menus`) declares for every action, without a wildcard arm,
whether its menu is built from the presented board (`Menu::Local`), is
content its host fetches live into an overlay that opens at the keypress
(`Menu::Overlay`: failed-check logs and dev-server logs), is a read a host
makes as a step of another request (`Menu::Host`, with the reason; no key
opens one), or is not a menu. Board builders are pure functions of `Board`
and answer any board, including an empty one. A board that cannot answer yet
returns `LocalMenu::NotYet`: the notice says so and the sources are asked to
read again. The host service refuses any action whose menu is board-built. An
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
Removing section, including records whose checkout is absent. With
`[ui] owed_sections` there is no Removing section: the row stays where it was
drawn, and the record's `landed` and `wasArchived` fields say where that is
once the checkout and its Git facts are gone. Only the current
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

Every action palette (`!`, `M`, `<` `>` `\`) is built from the board, for
local and remote rows. The catalog is `DisplayPolicy.actions`; a remote row
reads its own host's policy from `Board.host_display`. The action source
publishes `BoardRow.action_facts` (pull request fact, deployed stage, tracker
id, active run) and `Board.actions` (recent argument values, active runs by
slot, unavailable slots), and remote hosts' state arrives as
`Board.host_actions`. `wt_tui::blocker` is the one availability rule: the
palette lists entries by it and the host applies it again. An active run
opens the Stop confirmation with the run id the board shows. The TUI opens
argument and extras prompts itself and sends one `RunAction` carrying `seen`:
the row's path and branch, and the pull request number for an action that
requires one. The host makes one dispatch preparation and refuses as stale a
different checkout, a different pull request, a pull-request action whose
witness names no pull request, an entry unavailable now, or a run that
started meanwhile; the palette reopens from the newer board. Recent argument
values ride the board frame only for actions with an argument prompt.

A loading overlay (`Interaction::Loading`) takes every key while its request
runs. Esc, `q`, Ctrl+C or the overlay's own key close it and cancel the
request, and so does a dialog or request that replaces it. Replaced
overlays' request ids are kept in a bounded set of 8
(`Model.abandoned_overlays`), so their late replies open nothing. Nothing
typed is held or replayed. A 400 ms settle window (`Settle` in
`model/input.rs`) has two rules. After a menu key answers with a notice,
keys that act are ignored with a notice, quit included; moves, the mouse and
Esc pass. After a confirmation closes by commit or cancel, only a second
Enter, `y`, `n` or `q` is absorbed. A layer opened by key ends the window. A
confirmation reopened after a stale refusal carries the refusal as its first
line and takes no commit while it still names what was refused: it is
rebuilt when the board's witness changes and then settles, closes when the
newer board does not offer it, and is released after 5 seconds. On a stale
refusal the host re-reads sessions and the row's Git, and for GitHub asks
for the row's branch directly. A Stop confirmation whose run ended becomes
`ConfirmAction::Finished`, which only closes. `UiAction::ReadPullRequest` is
a background read on the Prefetch lane: `no PR for this branch` sends one,
and the row's host calls `GithubRefresh::refresh_live_branch`. An open confirmation rebuilds only while the rebuilt action
equals the one it opened for: when its subject changes on the board it stays
as shown and the host refuses the commit.
The header's refresh wave counts prepared refreshing lanes and has a 10 Hz timer
only while moving. Animation copies the last rendered screen and repaints its
header, without rebuilding pane models or requesting source work. Resize falls
back to a full render; modals and mouse selection suspend the animation timer.
The output pane switches between attention, all app activity, and prepared
session/action tails. Mouse-wheel events scroll the pane beneath the pointer;
the terminal restores normal mouse handling when the UI suspends or exits.
Worktree session keys use private tmux client exit codes for direct shell,
diff and agent navigation. The controller keeps the board suspended while the
owning host prepares the next session. The handoff resolves its worktree
once and retains the attached agent's tmux identity. A session key then reads
tmux only: the shell and diff keys check that their session exists and create
it when absent, and the agent key returns to the retained selection, whose
witness a replaced slot fails. A key refuses a checkout that no longer exists.
Entering a Claude session creates the slot and attaches without waiting for
the harness to register. SSH
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

Session pickers, the harness chooser and terminal stop are board menus for
every host. The session source prepares `Board.session_menus` and
`Board.terminals` for each local worktree and slot from the background
session-history snapshot and the one tmux inventory the board already holds.
`remote_board` lists each connected host's `session_menus` and `terminals`
under its rows' ledger keys, and a remote row's harness list and primary come
from `Board.host_display`. A terminal whose process exited and whose pane
tmux keeps is listed with `retained`, so its stop confirmation is built from
the board as well. Menus open from that board without a host request and
rebuild as discovery completes. A remote row whose host is disconnected or
has not sent its lists gets a notice (`session list unknown: <error>`,
`session list not loaded yet`) instead of a menu. Opening `;` also sends a
background `DiscoverSessions` read that queues a history refresh, so saved
conversations join the open picker. Cached history only nominates choices:
attachment and stop check the live tmux identity again, and a mismatch replies
stale so the menu reopens. A `New` choice carries the
live slot ids the picker showed; the host refuses it as stale when a slot it
did not show is live for that harness. Target resolution uses
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
A *keyed* leaf (`start_keyed_source`) lists its keys and reads each row with
its own call, a bounded number at a time: the Git leaf reads four checkouts
at once and keeps one of those lanes for user-caused reads. A row's stamp is
the earlier of its own read start and the start of the listing it was read
against. The domain stamp is the oldest row stamp, and a key the listing did
not show answers with the listing's stamp. Keyed stamps are filed under board
row keys (the GitHub leaf also files a branch's own read under
`//branch/<name>`, `github_source::branch_key`, which no slug or branch can
equal), and rows sharing a stamp key are confirmed together. `request_keys(Direct | Refresh, keys)` is user-caused and
`request_keys(Recheck, keys)` is event-caused. A request for the whole domain
is a sweep, except a whole `Direct`, which reads every row at user priority.
`start_demanded_source` hands a fetch the `Demand` it serves (whole, listing,
keys), `Fetched::keyed` publishes per-key stamps, and
`Confirmed::observed_for` reads one key's stamp.
`clippy.toml` bans raw
`tokio::sync::watch::channel` outside `wt-runtime`; a control channel that is
not board data says why with `#[expect(clippy::disallowed_methods)]`.

**Persistence.** Each leaf declares `Persistence::Disk(DiskCache)` or
`Persistence::Never(reason)`. Disk leaves (Git inventory, GitHub PR data,
review requests, the reviewer pool, issue statuses, dev status, session
discoveries, Git presentation facts, remote host snapshots) restore their last live value at
start, off the input path, as `Provenance::Restored` in the `Refreshing` state,
unless a live publish arrived first. They save live, ready data at most every
10 seconds and once at shutdown within a one-second budget that never fails
shutdown. A fetch shows its lane as refreshing only if it runs longer than 100
ms, so a skipped or quick refresh causes no redraw. Files live under `<cache root>/query/<name>.json` in an envelope
whose scope is a list of named fingerprints: the cache schema
(`QUERY_CACHE_SCHEMA`; for `dev-` builds also the build and the executable's
size and mtime), the canonical main clone, and each
configuration section a source's data can depend on. Sections that only decide
presentation or what wt does with the board (`[ui]`, `[[actions]]`,
`[[automations]]`, `[update]` and the like) are left out, so editing them
keeps the cache. The `discarding source cache` log line names the components
that changed. Remote host display snapshots use `<cache root>/remote-query/` instead:
their scope binds the repository, main clone, selected repository config,
endpoint, host protocol, and explicit display-cache schema. Compatible binary
updates and unrelated local settings do not discard remembered remote rows.
They restore as cached, never as confirmed live evidence. The saved copy
leaves out recent action argument values (`HostSnapshot::scrub_for_cache`
through `DiskCache::scrubbed`), which stay on the host that recorded them;
until the live stream arrives, an argument prompt for a remote row is a plain
text prompt. Empty startup frames
update connection state without replacing remembered rows. A mismatched,
corrupt, oversized or week-old file is deleted and ignored. The directory is owner-only, identical data is not rewritten, and
temporary files a crashed writer left behind are removed. Sources that read SQLite, logs or live process state are `Never`:
durable state is read directly and never cached here.

**Deploy proof.** The `deploy-proof` leaf (`Never`: live probe output) polls
each `[environments.<stage>]` probe and records the commit it reports as
`refs/wt/deployed/<stage>` in the main clone. The proof lives in Git, not
SQLite, so the Disk-persisted Git presentation leaf can read it. Every landing
computation (automation lifecycle facts, the board's Git facts) asks
`wt_ops::deploys::landing_deployed`, one `merge-base --is-ancestor` against
that ref, and `wt_core::Landing` combines the answer with the Git landing.
The leaf's snapshot only wakes those readers when a ref moves. A stage with
a probe and no ref yet is not deployed.

**Palette, reviewer and queue facts.** The `action-facts` leaf (`Never`: the
checkout and history files are the source) reads what the palettes need
beyond run records: recent argument values from the action history file,
which special slots are unavailable, and, only when some action requires
`deployed`, each confirmed local checkout's deployed stage. The deployed
probe runs on worktree and run changes, on a board refresh and every 60
seconds, so a deploy made outside wt can read as not deployed for up to a
minute. The `reviewer-pool` leaf (`Disk`, GitHub domain) holds the people
the reviewer picker offers. It reads GitHub when the pool is missing or a day
old, on hard refresh, and on a board refresh while there is no pool at all; a
restart inside the day reads nothing, and a pool that never loaded reports on
the attention feed. The derived `pr-facts` stage, after `work_presentation`,
attaches `Board.reviewers` and `PrPresentation.queue_removal`, and marks a
row with a queue removal as needing attention for as long as the reason
shows. The pool
lends the board no provenance: it only offers names, and its commit is
checked by the host. Queue removal is `wait_facts::queue_removal`, the
predicate `wt wait` and `wt fleet` call, and is set only from a live GitHub
read.

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
when the outcome is ambiguous. It is shown over confirmed data until a fetch of its confirming rows
(`Mutation::confirming`, by default its target; a change that names no row
waits for a fetch of the whole domain)
that started at or after the accept stamp shows the change, the write fails, or
a bounded deadline passes (60 s in flight; 30 s accepted for GitHub, reviews and
issues; 12 s for local domains; 30 s unconfirmed; creation, which can
legitimately take minutes, 10 minutes, and removal 30 minutes). An accepted
change stays past its deadline while no read that started after the write has
finished: up to 120 s for GitHub and 60 s for local domains. A read that
could have shown it and did not still expires it at the deadline. A fleet-wide cleanup is one entry: the
controller folds the owning hosts' answers into accepted (its own stamp, taken
after the last answer), unconfirmed only when a host's reply was ambiguous, or
failed. A change waits only on domains that a
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
  effect names, for Git exactly the rows the change names (`Effect::rows()`
  plus the created row from the reply), and replies with a `ReplyOutcome` (`Accepted(stamp)`,
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
  it does not delay agent entry. The action takes one of three install slots
  per lock directory and waits for one otherwise. When a selected row disappears, cursor fallback
  uses the current visual row identity and section, not a stale position.
  Creation captures that section and an anchor key. After checkout creation,
  the controller records the new layout and sends its own acceptance stamp to
  the TUI before the final creation reply. Layout and remote checkout creation use separate pending mutations,
  so neither host's clock confirms the other write. The `creationAfter` layout
  field applies while the row has no explicit order and, under status sorting,
  no work status. When the followed row leaves its section or is forgotten
  (moved, placed, reaped, its state cleared), a row that followed it inherits
  what that row followed, or gets a `creationSlot` (`{order, key}`, also in
  `remoteLayouts`) recording where the row was; a press that restates the
  anchor clears the slot; `prune_slug_records` writes the slot too before it
  drops a record. Layout resolves an anchor through the stored record of the
  row it names, whether or not the board lists that row (archived, landed,
  being removed, removed): while the record is filed in the section the unit
  stays where that row would be drawn. Otherwise it uses the slot, and only
  then puts the unit at the top. The slot keeps the leaving row's stored
  order, not its status, so under status sort the unit is drawn behind the
  unit with the nearest stored order. The projection preserves stack
  units and handles missing or cyclic anchors without dropping rows.
  J/K is resolved at the keypress: `plan_reorder` turns the press into a
  `ReorderStep` against the board on screen (`Pass` a neighbouring unit,
  `Cross` into the next section drawn, or `Section` for a header), and a
  press that moves nothing is a local notice with no host request. The step
  is projected at once as a `Pass`, `Cross` or `SectionPass` layout change. A
  `Pass` is keyed by its unordered pair of units and a `Cross` by its unit,
  so a later press on the same pair or unit replaces the earlier claim. The
  controller validates the step (every key is a row it lists, the pair is
  adjacent, `Cross.rows` are one stack, no derived section is involved) and
  passes the store each listed unit's band (its status under status sort)
  and whether it is drawn at its creation position; the store counts a unit
  as anchored only while it also has no stored order. The store compares the
  pressed band's drawn order with the stored one inside the same transaction
  and answers `LayoutChanged`, a stale reply, on a difference; nothing is
  written. When a press renumbers a section, only the pressed band keeps the
  drawn order; every other band is numbered by its stored order,
  never-ordered units first. The TUI's projection applies the same rule to
  creation positions, so the next press of a burst resolves from the board
  the read will bring. A row filed in a section but not drawn there (removed,
  archived, landed, or remembered in a `creationSlot`, which keeps a local
  row's status) stands in at the place it would be drawn for the rows created
  after it. A unit
  is the rows of a stack drawn together in one section, derived from
  `BoardRow.stack_parent` and `stack_index`, which layout publishes;
  `DisplayPolicy.status_sort` mirrors `[ui] sort`. J/K selection follows the
  pending reorder through unchanged snapshots and its confirming layout, and
  a refused crossing leaves the cursor on the row that did not move. Filing through the section picker, archiving, or
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
GitHub observations have separate source lanes. Filesystem changes invalidate
only the facts they affect. The Git leaf is keyed by checkout, so a change is
routed to the rows it can touch (a path under a checkout, that checkout's
`worktrees/<name>` metadata, a branch or upstream ref) and only those rows are
read again. A path under the worktree root in no listed checkout, or a ref no
row uses, asks for the listing alone, and so does a change reported for a
directory under `refs`, for the Git directory itself or for `objects`: the
refs, `HEAD` and index that moved are reported as their own paths.
`packed-refs`, `config`, `info/` and paths that cannot be placed ask for a
sweep of every row. A `config` or `packed-refs` inside a Git directory that is
rewritten with the same bytes is not a change: the router keeps a content
hash of each, and Git rewrites `config` with the same bytes on every fetch
that syncs a local branch. Files over 4 MiB, and any path that is not a
readable regular file, always count as changed. Working-tree paths with
those names are never compared. Ref routing ignores case on macOS and Windows.

The watch set is the worktree root, the main clone, the state database's
directory, each checkout's immediate children, and each Git common directory
with recursive `refs` and `worktrees`, excluding generated trees and symlink
traversal. On macOS a metadata scan every 500 ms is the only detector for
this set. Elsewhere native notifications drive and a scan every second
catches lost ones. A scan runs off the input path and has no entry or time
cap; one that runs long stretches its interval to four times its duration,
and one slower than its interval logs a warning, at most once a minute. Where
changes are found by polling, the state database, its WAL and the events
snapshot are also stat-checked every 75 ms. Metadata reads caused by other
processes' writes are paced to one per 250 ms after the first. A listing
waits up to two seconds for its checkouts to be watched and baselined before
their first read, so start-up normally makes one pass. A watch root that
appears asks for a whole-domain read only once the Git leaf has published
live rows. A full sweep runs every 60 seconds, on `r`, on hard refresh and at
start. Whole refresh requests within 2 seconds of the previous one are one
request, so `r` pressed twice is one sweep; an unrouted change and a
user-priority read of every row are never folded into it, and neither is an
unrouted change that arrives together with a refresh. Each completed
sweep logs one info record, `sweep completed`, with `source`, `rows`, `of`,
`duration_ms` and `cause` (`refresh`, `unrouted change` or `user action`).

Git reads are ordered: user-caused first, then event-caused, then sweeps. A
sweep read that has waited 5 seconds is served with the event reads. Sweeps
wait for a fleet-scan turn ([External effects](#external-effects-and-failures)),
and one process holds at most one turn, shared by its fleet reads. A board
refresh asks for an origin fetch unless one was asked for in the last 2
seconds. A
completed origin fetch asks for the checkout listing only. A finished
Git-affecting action re-reads its own checkout.
Native notifications for regular files already observed by that probe are
deduplicated against current size, modification time, inode, device and change
time. Directory and unknown-path events remain conservative invalidations.
The watch list derives from the Git source's inventory, minus checkouts with an
accepted removal record and checkouts that no longer exist; it is re-read when
either input changes. A watcher's missing-path error triggers at most one Git
refresh and watch-list re-read per five seconds; other watcher errors keep
their full refresh.
GitHub reads remain batched and rate
limited (on average one network read per 10 s; a background read also waits
10 s from the prior fetch start, stretched as the GraphQL budget each read
reports runs down, and a read the user caused may start one interval early; see
[github-events.md](github-events.md#graphql-budget)); the optional events
daemon supplies push updates stamped with its own fetch start, with a
configured poll backstop for missed events or daemon downtime. After an accepted
GitHub write the lane reads the written branch directly, its pull request and
its own merge-queue entry, and stamps only that branch newer; other
branches' queue entries, and whether the queue was read whole, stay as the
last read of every branch left them. While any of the worker's writes is
unconfirmed it keeps requesting that read (still rate limited) until one
shows the write or its deadline passes, so a lagging read model does not make
the change expire. A Sessions refresh also reruns the session-activity pass,
which re-reads the persisted primary harness.

`r` requests an ordinary source refresh and keeps durable data and derived
caches. `Ctrl+R` asks for confirmation, clears GitHub picker memory, bypasses a cached webhook snapshot once, and requests
fresh source data. It preserves authoritative SQLite state, running actions,
automation history, and agent-session identity. The performance pane is dormant
while closed; opening it takes a snapshot, and continuous sampling is opt-in.

## Host commands and remote execution

`wt_host::controller` accepts typed UI requests, assigns them to a host lane, and
tracks running commands through shutdown. `HostService` performs the host-local
operation and returns a typed reply. Accepted mutations belong to the command
drain, not the lifetime of a particular SSH view; read sources are cancelled
when their host scope ends.
Capacity is counted per host. Long jobs (whole-host operations, action
dispatch, and a removal until its worker is running) have four slots per
host. Pull-request and fork-base writes have two per host, session stops two,
and read-only preparation, the session picker's background discovery,
creation, metadata writes and host control one each. Three budgets are shared by all hosts. Attention
acknowledgement has a reserved view-control slot. Layout writes (fold, move,
reorder, rename or delete a section, archive) serialize with each other in
their own slot. Opening a link or an editor and copying exclude nothing and
wait only for each other's bound of four. Long host operations therefore
delay none of these. Terminal entry has no budget, only conflicts. History
and performance view state is not a request: the terminal publishes it as a
watch value and the controller applies the newest to every host. Attention
acknowledgement returns its own accept stamp before refreshing metadata.
Title, issue, status and notify writes share the one metadata slot of their
host. They conflict with removal
of their own target, including a fleet cleanup that removes it, but not unrelated
removals or cleanups, creation or long host actions such as compaction and GitHub requests. Transactional field
updates and captured revisions protect independent metadata writes. Palette title actions use the same classification as direct
title actions. Terminal handoffs serialize with each other;
worktree entry also conflicts with removal of that exact host and checkout,
whether single or part of a fleet cleanup. Special-session entry and unrelated checkouts remain
available during background writes. Preparations do not wait behind mutations because the
confirmed write revalidates its inputs. A removal excludes whole-host
operations and holds a long-job slot only until its durable worker holds the
checkout's lifecycle lock, which is when `start_remove` returns. From then it
excludes only work on its own checkout until it completes, and at most four
removals per host are in flight. A remote host's worker applies that planning
phase itself. Unrelated creates and removals proceed concurrently. A fleet cleanup is one accepted
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
Dispatching a configured action owns the one checkout it is for, or none for
a manager or slot prompt; two dispatches with the same scope on one host
still serialize. Restack, review checkout and restore own the host. A queued
long job is not overtaken by a later long job it conflicts with; short writes
and reads may pass it.
While commands run or wait, the controller sends nonterminal progress after
three seconds, including elapsed time and the conflicting operation or capacity
wait. These notices cannot settle an optimistic mutation or complete a menu
request. A failure or notice from an earlier request the user made is shown
while a later one is pending, and the failure stays beside that request's
progress and result until the next user action. Background reads never
interrupt.
The terminal sends user requests and its own background reads on separate
channels with separate queue bounds. The user queue holds 64 requests beyond
those the controller has taken. A key whose request finds it full is refused
with `actions are busy; try again when one finishes` and projects nothing. A
follow-up the model queued itself is not refused for a full queue: it is held
until there is room, and dropped with `a queued action was not sent: actions
were busy` once a key has acted, since it was meant for the view its reply
found. Every user request logs one info-level
`controller request` record (`operation`, `lane`, `host`, `queued_ms`,
`run_ms`, and `outcome`: `ok`, `failed`, `stale`, `uncertain`, `cancelled` or
`replaced`). A background read logs it at debug unless it failed, is
uncertain, or took over one second. Every loading overlay that is answered
logs `host overlay answered` with `operation_id`, `load_ms` and `filled`
(whether the reply filled it or closed it with a message).
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
`wt_app_core::watcher` selects the backend centrally: macOS uses `PollWatcher` with
metadata-only comparisons at a one-second interval, while other platforms use
notify's recommended native watcher. The Git and state watch set
(`wt_sources::freshness`) does not use this poller on macOS: its own metadata
scan is the detector there. Do not enable macOS kqueue here: recursive
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
`wt_ops::managed` builds a `wt_remote::managed::ManagedRequest` from the
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
limit before app composition. `wt_app_core::workload` converts the selected policy
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
state, and other user-authored records. It also holds each worktree's event
log, which is a record and never an input to a safety decision. SQLite schema migrations and the
versioned worktree-state payload migrations are distinct compatibility
boundaries. `wt state migrate` imports legacy records; it does not run as an
implicit cache clear. Opening the state database takes no write lock and
writes nothing once the schema and the repository row exist, and the state
payload is migrated at open. `Database::call` runs on the one writable
connection. `Database::read` runs on a separate read-only connection that
refuses writes, never migrates, and sees every write whose `call` returned
before the read started. It uses that connection only once the state payload
migration has succeeded in this process; until then it runs on the write
queue, which retries the migration first. The metadata leaf, row history and
the removed-rows ledger read through it. The metadata leaf reads state and
archived keys in one read transaction, and begins that read one stamp tick
(a millisecond) after its fetch stamp, so a write accepted in the same
millisecond as the stamp cannot fall after a read that counts as following
it.

The per-source query cache, event observations, action log tails, and source
snapshots are rebuildable. Hard refresh may clear derived caches; it must not
delete durable state or resend an accepted operation.
Action metadata and logs remain tracked on disk so a disconnected controller
can re-read completion and output.

Worktree removal and cleanup use the lifecycle service and per-worktree
operation locks. A confirmed removal keeps its row on the board, in the
Removing section or, with `[ui] owed_sections`, where it was drawn. Revision capture streams complete Git output and
untracked file contents into fingerprints with bounded memory, cancellation,
and deadlines; evidence is never truncated to satisfy a capture limit. Its
four Git reads (HEAD, status, and the unstaged and staged diffs) run
concurrently without optional index locks, the untracked list comes from the
status output, and untracked hashing runs on the long-job pool.
Untracked content that cannot be hashed (a nested repository, a socket or
FIFO, a set past the path or time budget) is pinned by name and stat data and
surfaced as a hazard the user confirms; it no longer makes removal impossible.
The worker archives the row before its startup reply and retains that state
on failure. A removal start returns once the worker holds the worktree's
lifecycle lock (`lockHeldAt` in the job record) or the job has ended; a
worker that acknowledged but reports no lock within 75 seconds leaves the
removal uncertain. TUI actions await the durable
job's terminal outcome off the input path, so startup does not settle the pending
removal. A later failure keeps it archived and reports an attention event.
CLI background mode returns the startup acknowledgement. Removal errors and completion enter the
attention feed. Cleanup does not force-remove dirty or unpushed work, and a
landed branch with an outstanding verification gate remains protected. Missing
GitHub or inventory facts are unknown, never proof that removal is safe.

Deleting a checkout's files is not part of removing it. A linked worktree
removed with a removal-revision witness is renamed to a `.wt-removing-*`
sibling right after the final witness check under the slug lock; that rename
is the commit point. Git then drops only that worktree's registration, the
branch and records are settled, the lock is released and success is reported.
The files are deleted afterwards: by the `_destroy` worker once it has
published its result, or by foreground `wt rm` after it prints the removal. At most two deletions
run at a time for one lock directory; a later one waits its turn.
The witness stands in for Git's own dirty check, so every removal without a
caller witness still runs `git worktree remove` in place, as does one whose
checkout cannot be moved aside or holds submodules. A Git-locked worktree is
refused. The mover holds an advisory lock on the quarantined directory itself
for as long as it lives; a later removal's worker sweeps quarantines nobody
holds, which covers a worker that died before deleting. A worker that died
between the rename and Git's bookkeeping leaves a registration with a missing
checkout, which the ordinary missing-checkout removal finishes. Phase timings
are logged under `wt::removal` (`quarantine-rename`, `git-worktree-remove`,
`quarantine-delete`).

Stacks are inferred from each worktree's recorded fork base. Preserve its
`baseSha`: it anchors squash-safe replay. A trunk fork base means the worktree
has no stack parent. “Landed” requires commits beyond that recorded base plus
the relevant Git/GitHub proof; reachability alone is vacuously true for an
unstarted branch. The proofs are tried in order: a pull request merged at
HEAD, the published base containing HEAD, a checkout that holds only copies
of the head its pull request merged from, then the base holding every patch
while merging changes nothing. With the copy proof the pull request's merge
commit stands for the work in the production and deploy proofs
(`wt_core::merged_pr_oid`).

## External effects and failures

Production process work goes through `wt-platform::process::ProcessRunner` with
an argv vector, working directory, timeout, output bound, and cancellation
token. Avoid shell construction for user values. Long-running child processes
and process groups are reaped by their owning scope. Expected domain errors use
the owning crate's error type; the CLI and TUI render actionable messages at
their boundaries.

One `ProcessBudget` admits the application's subprocesses by class
(`interactive`, `confirming`, `background`, `bulk`): 16 at once, 6 of them
reserved for interactive work, with background capped at 6 and bulk at 2.
Admission is first in, first out within a class. The highest class goes
first, except that a waiter older than 5 seconds goes ahead. A command's
timeout counts from admission. Waiting for admission has its own bound: 60
seconds, or 15 minutes for bulk, unless the command sets one. Heavy commands
(`git worktree add` and remove, Rift create, remove and gc, the destroy hook)
also take a slot shared by every wt process that uses the same
`paths.lock_dir`. A heavy command holds neither its local permit nor a
machine slot while it waits for the other: it waits for the permit and then
only tries the slot; when none is free it gives the permit back, waits for a
slot with nothing held, and keeps it only if a permit is free at once. Past
10 minutes, or the admission bound if that is shorter, it runs on its permit
alone. A read of every checkout's Git status waits at most 10 seconds for a
fleet-scan turn and then reads without one. Interactive handles take
neither. Dependency installs
(3 at a time) and deletions of removed checkouts (2) have their own pools in
the lock directory, outside the heavy slots. A wait of a second or more, or a
run of five seconds or more, logs `slow subprocess` under `wt::process`
(`program`, `class`, `wait_ms`, `run_ms`, `outcome`); a fleet-scan wait of a
second or more logs `waited for a fleet-scan turn`. A few helpers built
outside the application context (`wt init`, `_managed`, the controller bridge,
updates, the workload helper) run on a flat pool of 8 instead.

An origin fetch holds the repository's fetch lock for the fetch, its stamp,
and the ref and checkout maintenance that follows (restoring generated paths,
syncing the local trunk and `keep_fresh` branches, freshening idle Rift
clones). The lock is released before the main clone's dependency install,
which runs when the pull moved the main clone's HEAD and a lockfile differs
between the commit its packages were last installed at and HEAD. That commit
is recorded in `main-dependency-sync.stamp` in the repository lock directory
(`<git common dir>/wt-locks`): HEAD after a successful install, and the
pre-pull commit when no usable stamp exists, written before the install
queues for a slot, so an install that was cancelled while queued or failed is
still owed to the next pull that moves HEAD. Stamp writes take their own
short lock, `main-dependency-sync-stamp`, never held across the slot wait or
the install. The install runs as
bulk, not heavy. It takes one `dependency-install` slot in `paths.lock_dir`
first, then the `main-dependency-sync` lock, checks again under the lock, and
installs; a pull with nothing to install takes neither. A Rift create holds
`main-dependency-sync` while it copies the main clone
([backends.md](backends.md)).

Long blocking jobs that must not occupy the runtime's blocking threads run on
`wt_platform::blocking::run_long`, a process-wide pool of two threads. The
untracked-file hashing of a removal revision uses it.

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
admission remains cancellable and bounded (by the command's
`admission_timeout`, or else its timeout), while an admitted process runs to
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
