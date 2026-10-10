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
| `wt-runtime` | Scoped task lifetimes, refreshable source handles and source scheduling |
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
worker protocol. `sources.rs` composes the host-local source graph. A remote
host runs the same source and action pipeline as a local host; the controller
combines the resulting snapshots for display.

## Source and presentation boundaries

The TUI input loop consumes prepared `Board` snapshots. It does not run Git,
GitHub, harness, tracker, or filesystem scans while handling a key. Each
independent reader is a `wt-runtime` source with a `SourceHandle<T>` and a
refresh request lane. `start_source` applies its debounce and minimum interval,
publishes loading/ready/failed state, and retains last-good data on fetch errors.
`TaskScope` owns source tasks and cancels them together at the host boundary.

`wt-app::sources::start` builds the local inventory and metadata readers, then
projects GitHub, action history/log tails, naming, sessions, removed history,
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

The activity source backfills a bounded tail from the seven rolling native
JSON logs and manager reports. Attention transitions are written to that same
log with their event timestamp, so both feeds can recover them after restart
without a second durable event database. The attention watermark is stored in
the repository-scoped state and projected into every board snapshot.

`wt-tui::model` owns cursor, pane, modal, and key dispatch state. The terminal
driver polls input and prepared snapshots and renders them with Ratatui. Actions
cross a typed `UiAction` boundary into the application controller. This keeps
terminal rendering separate from I/O and lets the same host service handle
local and SSH-routed requests.
The output pane switches between attention, all app activity, and prepared
session/action tails. Mouse-wheel events scroll the pane beneath the pointer;
the terminal restores normal mouse handling when the UI suspends or exits.

Creation records the intended target and selects it only after the refreshed
inventory contains the actual row. A successful refresh alone is not proof the
row is visible. When a selected row disappears, cursor fallback is based on
the current visual row identity and section, not a stale numeric position.

## Freshness and refresh

Local Git inventory, metadata, edit timestamps, sessions, action logs, and
GitHub observations have separate source lanes. Filesystem watchers invalidate
only the facts affected by the event. GitHub reads remain batched and rate
limited; the optional events daemon supplies push updates, with a configured
poll backstop for missed events or daemon downtime.

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

## Durable state, caches, and operation safety

`wt-store` is the authority for repository-scoped status, sections, issue
overrides, fork bases, archives, removed-worktree history, automation pause
state, and other user-authored records. SQLite schema migrations and the
versioned worktree-state payload migrations are distinct compatibility
boundaries. `wt state migrate` imports legacy records; it does not run as an
implicit cache clear.

Generated naming summaries, remote snapshots, event observations, action log
tails, and source snapshots are rebuildable. Hard refresh may clear derived
summaries; it must not delete durable state or resend an accepted operation.
Action metadata and logs remain tracked on disk so a disconnected controller
can re-read completion and output.

Worktree removal and cleanup use the lifecycle service and per-worktree
operation locks. Cleanup does not force-remove dirty or unpushed work, and a
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
armed state. Retries retain the expected head SHA.

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
