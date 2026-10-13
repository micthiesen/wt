# Architecture

wt is a native Rust workspace. The installed `wt` executable is built from
`crates/wt-app`; running it does not require Bun, Node, a JavaScript bundle, or
a source checkout. The former TypeScript implementation is not included in this
tree; its behavior baseline is commit `d9cd2f4`.

## Workspace map

| Crate | Responsibility |
|---|---|
| `wt-app` | CLI composition, local and remote host services, TUI source pipeline, application actions, update integration |
| `wt-tui` | Board and modal models, input state machine, terminal rendering and action protocol |
| `wt-config` | Config discovery, merge, validation, resolved process configuration |
| `wt-core` | Pure identity, work-status, section and safety rules shared across domains |
| `wt-store` | Durable SQLite state, repository scoping, payload migrations |
| `wt-runtime` | Scoped task lifetimes, the query layer (sources with provenance, per-source disk cache, confirmed-only safety inputs, optimistic-change engine) and source scheduling |
| `wt-platform` | Bounded subprocess execution, locks and platform services |
| `wt-vcs`, `wt-stack`, `wt-lifecycle` | Git inventory/ref operations, stack replay, and worktree creation/removal safety |
| `wt-github` | GitHub reads and mutations, checks, review requests and merge behavior |
| `wt-harness` | Claude, Codex and OpenCode discovery, lifecycle, output and messaging adapters |
| `wt-actions`, `wt-automations` | Tracked action execution and typed automation evaluation/ledger |
| `wt-remote` | Native worker protocol and runtime transfer primitives |
| `wt-update`, `wt-launcher` | Verified release installation, activation, boot probe and rollback |
| `wt-skills`, `wt-events`, `wt-dev`, `wt-sst`, `wt-naming` | Skills distribution, webhook daemon, dev supervision, deployment reads and naming domain |

`crates/wt-app/src/main.rs` is the binary composition root. It resolves one
configuration for the process, opens the repository-scoped database, builds an
`AppContext`, and starts either a CLI operation, the local host, or the remote
worker protocol. `sources.rs` composes the host-local source graph; the GitHub
leaf's fetch scheduling (rate floor, events-daemon arbitration, deferred reads)
lives in `github_source.rs`. A remote
host runs the same source and action pipeline as a local host; the controller
combines the resulting snapshots for display.

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
The attention watermark is stored in
the repository-scoped state and projected into every board snapshot.

`wt-tui::model` owns cursor, pane, modal, and key dispatch state. The terminal
driver polls input and prepared snapshots and renders them with Ratatui. Actions
cross a typed `UiAction` boundary into the application controller. This keeps
terminal rendering separate from I/O and lets the same host service handle
local and SSH-routed requests.
The output pane switches between attention, all app activity, and prepared
session/action tails. Mouse-wheel events scroll the pane beneath the pointer;
the terminal restores normal mouse handling when the UI suspends or exits.
Interactive tmux panes retain nonzero command exits so startup failures remain
inspectable. Enter retries only a dead pane and clears its stale harness identity;
successful exits close normally. Retention is pane-local and enabled before exec,
never imposed on background action or development-server sessions. The session
source batches pane inventory with session inventory, reports new failed panes to
attention, and excludes dead panes from live-agent observations.

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
private, so nothing can publish restored data as live. `clippy.toml` bans raw
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
builds). A mismatched, corrupt, oversized or week-old file is deleted and
ignored. The directory is owner-only, identical data is not rewritten, and
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
  `NoChange`, `Deferred`, or `Uncertain`).
- Interactive session entry is a terminal handoff, not a pending board write.
  Shell, diff, harness entry and managed sign-in have no expected board
  postcondition or host write stamp; session sources independently observe any
  sessions they create.
  Connection progress and failures still arrive through normal action replies.
- Creation is a `UiMutation` too, but never inserts synthetic inventory rows.
  Progress and its F12 intent are tracked by operation id outside `Board.rows`.
  Inventory may publish the real checkout before its command replies without
  producing a second row. F12 requests agent entry once the host resolves the
  checkout identity; Esc or list navigation leaves creation focus. Other board
  controls remain available. The reply names the real row; selection moves to
  it only after the prepared visible rows contain
  it, and its section opens then. A successful refresh alone is not proof the
  row is visible. Dependency installation uses a durable background action, so
  it does not delay agent entry. When a selected row disappears, cursor fallback
  uses the current visual row identity and section, not a stale position.
- The GitHub action worker keeps a private ledger of its own accepted and
  uncertain writes for its no-op guards (disarm is sent whenever the ledger
  shows the PR armed; enable is refused while an earlier enable is
  unconfirmed). The ledger is never published, so automations and planners see
  only confirmed GitHub data.
- Host-side projections (issue statuses an action run is setting) use the same
  engine and publish `PendingView`s in `Board.pending`.

**Uniform display.** Every pane draws its border through
`wt-tui::render::status::pane`, naming the domains it shows; the title reads
`cached 3m`, `↻`, `loading…` or `<domain> failed` from the same per-domain
provenance on every host. Rows and the details title show pending changes with
one marker (`…` in flight, `↻` accepted, `? … (unconfirmed)`).

## Freshness and refresh

Local Git inventory, metadata, edit timestamps, sessions, action logs, and
GitHub observations have separate source lanes. Filesystem watchers invalidate
only the facts affected by the event. A one-second metadata backstop detects
lost Git notifications over the same declared watch paths: immediate checkout
children and recursive Git refs/worktree metadata, excluding generated trees
and symlink traversal. Each scan is capped at 8,192 entries and 200 ms, runs off
the input path, and refreshes only the domains whose file metadata changed.
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
refreshing metadata.
Quitting closes command admission and waits for accepted local writes to finish;
after 30 seconds it reports that it is still waiting. Each operation owns its
timeout and safe stopping points, so a UI deadline cannot interrupt a write.
Known create failures retain the input and selected host for correction. A lost
remote reply remains ambiguous and never opens an automatic retry path.

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
The worker saves the archive state before its startup
reply and restores the previous state on failure. TUI actions await the durable
job's terminal outcome off the input path, so startup does not settle the pending
removal and a later failure rolls it back. CLI background mode returns the startup
acknowledgement. Removal errors and completion enter the
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
