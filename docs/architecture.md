# Architecture

Internals map for contributors and coding agents. Bun + React + [OpenTUI](https://github.com/sst/opentui) on top of TanStack Query. The companion rules file for agents is [`AGENTS.md`](../AGENTS.md); this page is the *map*, that one is the *rules*.

## The three layers

The TUI is split into three layers; respect the boundaries:

- **Sources** — `src/state/queries/` (per-source files behind the `src/state/queries.ts` barrel), `src/state/hooks.ts`, `src/tui/hooks/useWorktreeRows.ts`. They own fetching, batching, and caching via TanStack Query. Small fixed set (github, git, sst, dev-server, claude, issue-tracker-derived, ai); not user-pluggable.
- **Rows** — `src/tui/rows/*.tsx`. Pure-presentational modules declaring `{id, label, sources, render, visible?}`. Multiple rows can read from the same source; the source still fetches once. `src/tui/rows/index.ts` is the registry; `[ui].rows` in the user config selects + orders them, and a row hides itself when its integration isn't configured.
- **Driver** — `src/tui/panels/details.tsx`. Iterates the configured row list, computes the trailing staleness glyph, and renders inline errors verbatim once retries are exhausted. Also owns the pane-level chrome that isn't a row: the resolved title in the border bar (`paneTitle`, hand-truncated — opentui's native drawBox drops an over-wide border title instead of clipping it) and the AI description band below the row stack.

The list panel (`src/tui/panels/list.tsx`) is deliberately **not** row-driven — different layout (one line of glyphs, no labels). Don't try to unify them.

## Effect boundary

Effect 4 (`effect@4.0.0-rc`) is the model for production work that performs
I/O, starts or waits for subprocesses, coordinates concurrency, retries, waits
on time, or owns a resource. These functions return `Effect<Success,
ExpectedError, Requirements>` under their plain name and compose until one of
the runtime boundaries runs them:

- `src/main.ts` for CLI and process lifetime (`Effect.runPromise` once, then a
  hard exit);
- TanStack query and mutation functions, through `runQuery(effect, signal)` in
  `state/queries/boundary.ts` — the context's `AbortSignal` interrupts the
  fiber, so a superseded key cancels the git/gh subprocess under it instead of
  letting the stale fetch finish against the shared `run()` budget;
- OpenTUI key handlers and flow callbacks, through `forkReported(effect,
  report)` in `tui/effect-boundary.ts` — fire-and-forget on purpose (the action
  should outlive the modal that started it), with the failure toasted and
  logged by the caller's reporter rather than thrown into React;
- React lifetimes, through `useEffectFiber(make, deps)` — one fiber per mount,
  interrupted on cleanup;
- worker entrypoints (`_`-prefixed CLI commands, the diff and tail workers);
- tests.

Naming: the Effect takes the plain name. A Promise-returning adapter exists
only where an external contract requires one (the TanStack persister, a
`Harness` interface method, a worker message) and is named `fooPromise`, beside
the Effect it runs; a synchronous adapter that forks a fiber and returns at once
(an `fs.watch` callback's `cancelUnsafe`) takes Effect's `Unsafe` suffix. Untyped boundaries — a synchronous call that may throw, a
Promise API, a dynamic `import()` — are wrapped once with `operationErrors(source)`
from `core/errors.ts`, whose `OperationError` carries `source`, `operation` and
`cause` and renders as `operation: cause`. Domain failures keep their own
`Data.TaggedError` classes with fields consumers match on, and every tagged
error overrides `message`, because the crash renderer, the CLI and the row
error prints all read it. Defects remain defects.

Named effectful functions use `Effect.fn("name")`; module-private helpers and
generic functions use `Effect.fnUntraced`. Use Effect scopes and finalizers
for watchers, subprocesses, timers, workers, and subscriptions; Effect
concurrency primitives (`Semaphore`, `Ref`, `Deferred`, `Queue`, `FiberMap`)
for shared limits and in-flight work; `Schedule` for retry and polling; and
`TestClock` from `effect/testing` for every time-dependent test. Services and
layers are introduced only where a real seam exists (a dependency a test or a
second implementation actually swaps); the existing hand-rolled `deps` objects
(`createSessionMessenger`, `DaemonDependencies`, `CleanDeps`) are that seam and
stay as they are until a second consumer asks for a `Layer`.

Pure synchronous transforms, synchronous SQLite/fs reads used from render
paths, and React's own lifecycle hooks remain plain TypeScript.

`bun run lint` runs the official language-service diagnostics and `bun run
typecheck` runs them again through the patched compiler. The rules this project
relies on are promoted to warning severity in `tsconfig.json`; at their default
suggestion severity they never reach a CLI run, so a clean lint proves only what
was promoted. (`effectFnOpportunity` is inert in the installed plugin version
even when promoted; `Effect.fn` adoption is a review habit.) `bun run build`
verifies the Bun production bundle. When unsure about an API, the authoritative
sources for the installed version are `node_modules/effect/AGENTS.md`,
`node_modules/effect/ai-docs/src/index.md` and `node_modules/effect/dist/*.d.ts`;
the narrative docs live at
[effect.website/docs/v4](https://effect.website/docs/v4/), including
[typed errors](https://effect.website/docs/v4/error-management/expected-errors),
[resource management](https://effect.website/docs/v4/resource-management/introduction),
[concurrency](https://effect.website/docs/v4/concurrency/basic-concurrency),
and [code style](https://effect.website/docs/v4/code-style/guidelines).

## Composition root

`src/tui/app.tsx` wires everything: state declarations, hook wiring, per-render flow factories, the ctx objects key handlers destructure, and the layout JSX. The pieces:

- **Keyboard** — `src/tui/keyboard/` (`global-keys.ts`, `footer-input-keys.ts`, `removed-view-keys.ts`, `normal-keys.ts`) plus `src/tui/modal-keys/` (one file per modal family; `index.ts` is the dispatcher). The `useKeyboard` callback in app.tsx only routes, in load-bearing order: modal → footer input → removed view → `h` toggle → normal mode. Handler-check order *inside* `normal-keys.ts` is also load-bearing (see its header comment).
- **Flows** — `src/tui/flows/` (`destroy.ts`, `sessions.ts`, `github-pr.ts`, `sections.ts`, `base.ts`, `reviewers.ts`, `new-worktree.ts`, `action-picker.ts`, `perf-report.ts`, `error-report.ts` — per-render factories over a context object) and `src/tui/hooks/useActionDispatch.ts` (action launch + completion subscriber). New flow logic goes in a flows module, not back into app.tsx.
- **Modal overlays** — `src/tui/modal-host.tsx` (`PreFooterModals` mount before the Footer, `PostFooterModals` after; render order is paint order). The modal union lives in `src/tui/modal-state.ts`; `modal.tsx` is the shared chrome component.
- Pure helpers in `src/tui/app-helpers.ts`; title-bar badges in `src/tui/usage-badge.tsx`.

## Module layout conventions

The big core modules are directories behind a same-named flat barrel: `core/github.ts` → `core/github/`, `core/wtstate.ts` → `core/wtstate/`, `core/stack-ops.ts` → `core/stack-ops/`, `core/actions.ts` → `core/actions/`, `core/tmux.ts` → `core/tmux/`, `core/skills.ts` → `core/skills/` (the skills-distribution system — [skills.md](skills.md); its interactive prompt flow lives CLI-side in `cli/skills-sync.ts`, shared by `wt skills sync` and the pre-TUI startup check in `main.ts`), `state/queries.ts` → `state/queries/`. The barrel re-exports the module's public surface with explicit named re-exports — importers keep using the flat path; only names in the barrel are public. (`tui/modal-keys/` is a plain directory — its single consumer imports `index.ts` directly.)

Per-harness code (Claude/Codex/OpenCode session discovery, naming, events, usage, tails) lives under `core/harness/<harness>/` behind the generic `Harness` interface (`core/harness/types.ts`); `core/harness/status.ts` is the shared `DerivedState` vocabulary.

`HARNESSES` is the complete capability registry; `VISIBLE_HARNESSES` applies
`[harness].hidden` for automatic routing and TUI surfaces. Explicit CLI
addressing and lifecycle cleanup use the complete registry, so hiding a
harness is a personal presentation preference rather than removing support.

The GitHub events daemon stamps its loaded wt build into `events/state.json`.
`core/events/startup.ts` checks that stamp on every interactive startup and
restarts an installed stopped, stale, or pre-stamp daemon out of process. The
check belongs to the fresh startup rather than the old updater process, so the
first upgrade from a version without the hook repairs itself too.

Codex and OpenCode session UUIDs are harness-owned resume handles, while wt owns a persistent per-worktree `primary` / `2` / `3` name mapping. That mapping is identity, not presentation: the picker shows it, F12 resumes the mapped `primary` when no session is live, and detached cold starts used by `wt agent send/start` resolve the same UUID before spawning. When several harnesses are live on one worktree, the Shift+Tab-selected primary harness wins the F12 target and list glyph; recency only breaks ties inside that bucket and is a fallback for unmapped legacy data. Explicit `+ new` picker rows are the only path that intentionally starts a fresh single-slot conversation.

Codex's `main` and `manager` slots share a cwd, so `core/harness/codex/slot.ts` stamps new conversations with a fixed opening user message and derives ownership from the opening turn. Unmarked legacy conversations belong to main after their first assistant message; incomplete or unrecognized prefixes belong to neither shared-cwd slot. Worker-side discovery and both output readers apply the same ownership filter before naming/selecting rollouts. The reader scans at most 2 MiB of the opening prefix, caches completed classifications, and retries incomplete writes. Ordinary worktree discovery remains cwd-based. Keep the opening messages stable: their exact text is the persisted identity contract, not a display label.

**The CLI dispatcher imports lazily.** `cli/index.ts` maps each subcommand to a `() => import("./commands/<name>.ts")` thunk, so `wt <cmd>` loads that command's module graph and nothing else (35 modules for `wt status`, against 153 for all commands at once). This is containment, not speed: users update hot from main, so any push can put a broken module in front of every agent on the machine, and a static barrel turns one bad export into a total outage — which is exactly what happened, taking `wt status` down with the transport it doesn't use. Commands whose branches differ in what they need split further: `wt manager report` imports no session machinery at all, so the fleet keeps its ability to report that delivery is broken. `scripts/broken-module-check.sh` asserts the property by breaking a module in a throwaway copy of `src/` and printing which commands survive. `main.ts` still routes `update`/`rollback`/`version` around the dispatcher entirely, because those must work when the dispatcher itself is what failed to parse ([updates.md](updates.md)).

Claude session lifecycle lives in `core/harness/claude/sessions.ts`. A target is the canonical cwd plus its deterministic wt conversation UUID and optional managed name. `ensure` serializes cold starts under a per-session lock and reuses the normal detached tmux host. Discovery is Claude's own per-process state directory (`core/harness/claude/registry.ts`), which already drops entries whose pid is gone — so there is exactly one liveness authority and nothing of wt's own to keep in sync. Claude stop hard-kills the hosted session rather than sending control keys.

**Message delivery** is `core/harness/session-messaging.ts`, the single choke point every send funnels through (CLI, TUI flows, actions, automations). It owns three things: stamping the sending agent's slug (`WT_AGENT`); serializing per target conversation (the manager slot is a genuine multi-writer singleton, and two overlapping injections each restore their own captured draft on a timer); and a two-rung transport ladder — prompt injection (`core/harness/claude/inject.ts` → `inject/`, ported from unseamless-coop: connect to the session's bun inspector socket, walk the live Ink/React tree, call the prompt's `onSubmit`) falling back to terminal input (`core/tmux/inject.ts`) with an attention line naming the failure. The same per-target lock holds an outbound send while Claude's registry reports `waiting`: the active sender waits interruptibly until the human-facing question or permission dialog closes, so no prompt answers the dialog and later senders stay serialized behind it. `createSessionMessenger(deps)` exists so the ladder is testable without a live session. The socket is opened by `BUN_INSPECT` in the pane env (`core/tmux/inner-process.ts`, the one place both the attached and detached creation paths build it), under a 0700 `insp/` dir in the cache root; `inject/shims.ts` keeps that variable from leaking into the session's own bun children, where inheriting it is fatal rather than noisy (a bun-compiled CLI exits 1 having done nothing). Which commands get a shim is *discovered* — every executable on PATH whose Mach-O header names a `__BUN` segment, minus `NEVER_SHIM`, the harness binaries wt launches sessions as — and the shim directory is *pruned* to that set on every spawn, because PATH resolves the directory and a shim no longer wanted would otherwise keep being used forever. Discovery has one structural blind spot, covered by the named `LAUNCHER_SHIM` floor: **package-manager launchers are not bun programs, so they carry no `__BUN` segment, and they resolve their targets from `node_modules/.bin` which they prepend ahead of the shim dir.** So `pnpm exec supabase` bypasses a shimmed `supabase` entirely — and that is the form a repo's own docs recommend, since it pins the CLI version from package.json. Shimming `pnpm`/`npx`/`yarn` themselves covers every binary they will ever resolve. Each shim also falls back to re-resolving its command on PATH when the baked path has vanished (fnm hands out a per-shell directory), dropping both the generated dir and its own `$0` dirname so it cannot exec itself. Semantics and the fallback rules: [manager.md](manager.md#how-a-message-reaches-a-session).

Interactive harness wrappers unset both `NO_COLOR` and `NO_COLOUR`. A caller's plain-output preference must not disable styling in a newly launched Claude, Codex, or OpenCode TUI.

Worktree **backends** follow the same shape: `core/backend.ts` → `core/backend/` behind the narrow `WorktreeBackend` interface (`create` / `remove` — the only two filesystem mutation points, extracted from `lifecycle.ts`). Two built-ins: `git-worktree` (linked worktrees, one shared object db) and `rift` (copy-on-write clones). Everything else wt does to a worktree (fork-base record, env/configured-glob copy, stage pin, upstream, status) stays backend-agnostic in `lifecycle.ts` / `worktree.ts`. `getBackend(kind)` picks the create backend from config; `getBackendForPath(path)` derives the owning backend from disk (a `.rift` marker) so removal is correct after a config flip. This is the LOCAL-materialization axis, orthogonal to any remote (SSH-host) axis. See [backends.md](backends.md).

## Freshness model

Freshness is **push-based**; the `r` keybind is a backstop, not the mechanism. Every external state source has an event trigger that invalidates the matching query:

| trigger | invalidates |
|---|---|
| `.git/refs/` watcher (commits, fetches, pushes) | github + per-worktree fields + wtState + reviewRequests (deliberately keyed outside the `["github"]` prefix) |
| `.git/worktrees/` watcher (worktree add/remove) | worktree list |
| worktree-root watcher (subdir add/remove) | worktree list — catches `rift` checkouts, which are independent clones that never touch `.git/worktrees/`; harmlessly redundant for git worktrees |
| `.git/worktrees/<slug>/rebase-{merge,apply}` watcher (hand/`/restack` rebase starts or ends) | that slug's conflict probe (the mid-rebase glyph) |
| per-worktree dir watchers | edits → dirty; `.sst/` writes → deploy |
| `~/.local/state/wt/wt.sqlite` WAL watcher | cross-process fork-base / section / archive writes, partitioned by repository id |
| `~/.cache/wt/locks/` watcher | per-slug busy state from any process (create/destroy, and every chain member during a restack — the restack glyph rides on this); a release also fans out a per-slug field refresh (`useLockReleasedInvalidator`) **and refreshes the worktree list** — the reliable "a create/destroy just finished" signal, so a new (esp. `rift`) row surfaces immediately instead of waiting on the interval (a rift `.rift` marker is written inside the new dir, after the worktree-root watcher already fired on the bare dir) |
| github-events webhook marker | github + a forced `git fetch origin` |
| 3-minute `fetch origin` interval | backstop for remote drift |
| 15-second remote-worker handshake | worker role / protocol / build — clears a version warning after `sync-wt` without restarting the controller; concurrent inventory fetches share the same SSH request |
| **any** `fetchOrigin()` — the interval, the webhook, `wt new`, a restack replay, `wt ls`, `wt clean` | every worktree's own `refs/remotes/origin/<trunk>`, fast-forwarded to the tip the main clone just fetched (`freshenWorktreeTrunkRefs`). A no-op under `git-worktree`, where the ref store is shared; under `rift` each checkout is an independent clone that nothing else ever fetches into, so the ref decayed from creation and every base-derived surface read it — counts, the pre-PR title, the diff context, the conflict probe, `{{base}}`, and the agent's own `git log` in the checkout. See [backends.md](backends.md#stale-remote-tracking-refs) |
| **any** `fetchOrigin()` (same set) | the local heads named by `[branch] base` and `[branch] keep_fresh`, fast-forwarded in the main clone. `git fetch --prune` already moves every `origin/<branch>`; nothing was moving the local ones |
| **any** `fetchOrigin()` (same set) | the module-level first-parent SHA cache in `core/git.ts` (not a TanStack query). Invalidated *inside* `fetchOrigin` rather than by callers: that set is what tells `branchIsMerged` "this tip is just an older trunk commit", so a stale one makes a branch forked at the new tip read as landed work — and `merged` closes GitHub issues and feeds the clean sweep. Five of the six callers used to skip the invalidation, `wt new` (which fetches immediately before forking) among them |
| 3-minute github `refetchInterval` (poll-only setups; the webhook daemon's own backstop replaces it when configured) | github — the one source whose interesting changes (a comment, a review, a check finishing) happen entirely on GitHub's side and move nothing locally, so every *local* trigger can stay quiet through them |
| claude-registry fs.watch, session-tail triggers (`gh pr …` / `git push` inside a session) | sessions / github / claudeUsage (a registry rewrite IS claude activity, exactly when API utilization changes) |
| `tmuxSessionsQuery`'s `dev` set (batched tmux read, 5s poll + push-invalidated) | `wtDevQuery`'s session-liveness half — the value is part of that query's key, so a session start/stop cache-misses into an immediate refetch instead of spawning a redundant per-worktree `tmux has-session`; the port-probe half keeps its own 15s poll as backstop. That probe is three-valued (`probePort`: listening / free / unknown) — on loopback only `ECONNREFUSED` means "nothing there", so a timeout is reported as `unknown` and a live server is left alone rather than being flipped to stopped |
| codex/opencode activity-poller ticks (`startCodexEventPolling` / `startOpencodeEventPolling`, the same 2.5s tickers that feed the activity pane) | codexUsage / opencodeCost, on ticks that actually observed an event — token usage and spend change exactly when those sessions are active, so this rides the existing sensor instead of leaving the query poll-only. Codex usage scans every date partition because a resumed session keeps its original path, and classifies rate-limit windows by `window_minutes`; `primary` is a slot, not a duration, and some plans expose only one window. |
| action `affects` tags on completion | the declared domains (`git`, `github`, `dev` — the dev-server start/stop builtins declare `dev`, refreshing both the slug's fields and the batched tmux-session source that drives the dev query key; a 15s poll backstops out-of-band crashes) |
| manager-reports spool watcher (`~/.cache/wt/manager/reports.jsonl`, written by `wt manager report`) | nothing query-shaped — new lines are narrated straight onto the attention feed (`useManagerReports`; 10s poll backstop). The footer's manager context % is likewise push-based, riding the session-tail registry's per-turn `lastUsage` rather than any query |

A second, smaller exception rides an existing query: `wtDevQuery` also reports whether the slug is queued behind `[dev_server] max_concurrent`, read from the waiting-room dir on each fetch. That half is interval-only (the query's own 15s poll) with no watcher, deliberately — a wait lasts minutes and joining a queue is not worth an fs watcher of its own.

One deliberate exception: `perfSnapshotQuery` (the `P` overlay) polls as its *primary* mechanism, not as a backstop. Nothing emits an event when some process starts burning CPU, and the overlay's whole job is to show the number moving. It's gated hard on the modal being open (`enabled`), so it samples at 2s while visible and not at all otherwise, and it's excluded from the persister — a restored snapshot is a previous run's dead pids. Don't treat it as precedent for polling a source that *does* have a trigger available.

When adding a new state source or mutation path, wire one of these (or an explicit invalidation at the call site) rather than shortening a staleTime — staleTimes only bound how wrong things can be when a trigger is missed. Watchers live in `src/core/repo-watch.ts` and are wired in `src/tui/runtime.tsx` through a 50ms-coalescing invalidation scheduler.

**A refresh has a size, and the big one is not free.** `invalidateQueries(["wt"])`
— the per-worktree wave inside `refreshAll`, i.e. what `r` does — refetches every
field of every row: `worktrees × 10` git probes issued in one burst. `Bun.spawn`
runs its `posix_spawn` synchronously on the calling thread, so that burst is a
render-thread stall before it is background work (measured: blocks up to 2.7s on
a 22-row board). Two things keep it in hand. `run()` in `core/proc.ts` caps
concurrent subprocesses (`RUN_CONCURRENCY`), which spreads the spawns across
event-loop turns and took the same refresh to a 185ms worst block. And mutation
paths reach for the SCOPED refresh that matches what they changed rather than the
wave: a destroy changes which worktrees exist, not the state of the survivors, so
`doRemove` / `doCleanRows` call `refreshAfterRemoval` (list + wtState) — the
github query re-keys itself off the shorter branch list, and the watchers above
carry the rest. Reach for `refreshAll` when the user asked for "everything", not
as the tail of an operation you can describe precisely.

Three related invariants:

- The github source is **one batched GraphQL fetch** aliasing every per-worktree PR field plus the repo merge-queue block. New PR fields go into `PR_FRAGMENT` in `core/github/fetch.ts`, never a separate query. "Batched" means never per row; it does not mean one HTTP request. The query doc is split into fixed-size chunks of `CHUNK_SIZE` (8) branches that run concurrently, because GitHub caps server-side *execution time* at roughly 10s per query while this query costs ~250ms per branch — so a single request crossed the ceiling around 35 branches. Splitting costs nothing on the budget that is actually metered (GraphQL points come from node count: 27 points for 24 branches either way) and cuts wall clock roughly in half. The merge queue rides exactly one chunk, and is queried for `branch.base` explicitly: a merge queue belongs to a BRANCH, and the unqualified `mergeQueue` field resolves to the repo's DEFAULT branch, so a repo queueing on `staging` while defaulting to `main` reported no queue at all.
- A chunked fetch **fails whole or not at all**. Each chunk retries on transient failure (5xx, GitHub's timeout copy, HTTP/2 CANCEL, truncated body) and never on a rate limit; if one still fails, `fetchGithub` throws rather than returning its siblings' data, because partial results would blank the PR badge on exactly the failed chunk's branches — indistinguishable from "no PR there". Retries live in the core module, so only the failed chunk re-runs and the CLI callers and webhook daemon inherit them; the query client's global `retry: false` still governs everything a keystroke drives.
- Anything that *mutates* GitHub state must invalidate `["github"]` (via `refreshGithub()` in `state/hooks.ts`), not the worktree — the github query is keyed by branch list, not slug.

## Controller and worker execution

The optional `[remote]` host runs the same wt source in explicit
`[instance] role = "worker"` mode. It owns its clone, worktree paths, fork
bases, agent statuses, locks, and tmux/dev processes, while the controller owns
the single visible TUI and every presentation decision. `remoteWorktreesQuery`
handshakes role + protocol + build, polls the host's versioned `_snapshot`
endpoint, and renders
those summaries—including agent lifecycle assertions and the worker-observed
dev-server state—in the controller-owned manual section (or Inbox), alongside
local rows. Sessions receive that worker checkout's `bin/wt` at the front of
`PATH`, so `wt status` writes to the same worker state the inventory reads;
the controller narrates newly polled assertions through the same attention-feed
rules as local state changes.
Remote filesystem paths are never
accessed as if they were local.
The query's successful inventory is persisted for offline startup and retained
across refetch failures. SSH failure changes host health only: the host header
renders a warning and session keys are disabled until a later poll succeeds.

Fleet identity is separate from checkout access. `core/worktree-ref.ts` gives
every row a location-aware ledger key: local rows retain their bare slug for
on-disk compatibility, while remote rows use the stable SSH destination plus
slug. `core/worktree-target.ts` is the shared row-facing target shape: common
slug/branch/path/stage metadata plus a `local` or endpoint-carrying `remote`
location. The list/cursor model builds one of these for every selectable
worktree; feature code should branch only at its I/O boundary.

The cursor itself is a KEY (`sel` in `app.tsx`), resolved to an index by
`useVisualItems` — so it tracks a row through re-sorts rather than a
position. Two rules cover the cases where that's the wrong default.
Actions that take the selected row out of its slot (`d`, the `c` sweep,
`a`, the `l` section move) call `advanceCursorPast` FIRST, which asks
`cursorSuccessor` (`tui/app-helpers.ts`) for the nearest survivor in the
same section — skipping the rest of a sweep's candidate set and anything
already archived — and re-points `sel` at it; without that step a destroy
drags the cursor into the archived block at the bottom of the board,
where the row parks for the length of its teardown. When a row instead
vanishes with no wt-side action behind it (an external `wt rm`, another
instance), `useVisualItems` holds the cursor at the same visual index and
an effect in `app.tsx` adopts whatever now occupies it, so the selection
is a live key again instead of a dead one that drifts on the next
re-sort. See [tui.md](tui.md#navigation) for the user-facing statement.

Creation uses a pending selection key instead of selecting a key before the
inventory has rendered it. It appends the persisted section order, expands the
destination section, and selects only once the real row is in `visualItems`.
The initial bottom placement is a TUI-local, write-once snapshot of the row's
work-status timestamp and manual layout. `buildActiveItems` appends matching
rows after both local and remote peers; a changed status or layout releases
the override back to normal sorting. It does not alter the inferred stack.

Presentation/coordination state owned by this TUI (sections, ordering, folds,
and the archive ledger) uses the location-aware key, so remote rows participate
like local rows without ever making their paths look local. The worker's own
legacy section value is ignored and worker-mode fleet JSON reports `null` for
it. Operations that need the checkout
itself dispatch by target: direct calls for local rows, the target's captured
endpoint for SSH rows. Remote query caches are likewise keyed by SSH host, not
by the singleton config slot or display label. These are deliberate
multiple-remote invariants even though the config currently accepts one
`[remote]`: adding a second host should mean producing more targets/queries,
not migrating identities or teaching features about a second remote-only model.

`core/remote.ts` drives SSH, while `core/remote-protocol.ts` base64url-encodes
the complete argv into a single shell-safe token. The remote `_remote` CLI
entrypoint decodes that token and re-enters normal dispatch, avoiding any
dependency on remote login-shell quoting. `_hello` is the compatibility
boundary: the worker must report the worker role and matching protocol before
inventory or commands are trusted; its build string is compared separately so
same-protocol development snapshots warn rather than fail.
`_snapshot` is the protocol's sole inventory contract. It returns the nested,
location-neutral `WorktreeSnapshot` (`status`, `work`, `dev`, and Git facts),
while `wt ls --json` remains a public compatibility view over the same
collector. Controller layout, archive state, GitHub data, and endpoint
coordinates are joined locally and never enter the worker snapshot.

`Ctrl+N` forwards `wt new` and refreshes the remote-row query when creation
finishes. Creation keeps the current selection and adds no placeholder.
New inventory identities on the creating host are withheld while the command
runs, including results from background polls; existing rows and other hosts
stay visible. Completion refreshes inventory and selects the discovered row,
and a finalizer releases the visibility hold on success or failure.
F10/F11/F12 on a remote row use the hidden `_session` entrypoint;
Cachy runs that one worktree's tmux session while `renderer-handoff.ts`
suspends the Mac renderer. Detaching returns to the same Mac Inbox.
`a` writes the location-aware key to the Mac's archive ledger; it is a view of
this fleet, not a mutation of the remote checkout. `d` forwards the normal
`wt rm` command after confirmation, preserving the remote installation's lock
and dirty-work safeguards while explicitly leaving any SST stage intact. The
dispatch is not gated by cached inventory health or busy state: those can be
stale, while the bounded SSH call and remote lock are authoritative. `c` builds
one confirmation from location-tagged local and remote candidates, then routes
each removal at its I/O boundary; candidate identity already includes the host,
so the flow remains valid when multiple remotes are added.

## Modal UX rules

Every list-picker modal follows the same shape so muscle memory carries across pickers — and the shape is now CODE, not convention: `tui/modal-keys/list-picker.ts` (`handleListPickerKey`) implements move/digits/chords/confirm/cancel once, and every picker handler delegates to it after its picker-specific pre-checks (text-input modes, space-toggle, preview-on-move). Add new pickers through it; hand-rolling the base keys is how pickers drift. The rules it encodes:

- **Trigger-key re-press confirms.** Whatever key opens the picker (`l`, `;`, `'`, `!`, `M`, `v`, `b`, `u`, `y`, `Shift+F12`) also commits the highlighted row when pressed again (`l l`, `; ;`, `' '`, `! !`, `M M`, `v v`, `u u`, `y y`) — the `confirm` option. Shifted-letter triggers work through `matchesTrigger`'s `isShiftedLetter` leg (csi-u never delivers the uppercase literal in `sequence`).
- **Enter still works** — the chord is the cheap path, Enter the discoverable one.
- **Esc / q / Ctrl+C cancel.** Universal, no exceptions.
- **j/k or arrows move.** Nothing fancier; `g`/`G` aren't bound inside pickers.
- **1–9 quick-pick** when the list shows ≤9 items; out-of-range digits are ignored. Pickers whose rows have their own letters (actions) or where digits would be ambiguous (multi-select) pass `digits: false`; pickers with special rows remap via a `digits` function. When digits are live, the hint bar says so (`1-9 quick pick`) — working-but-invisible keys are how the convention stopped being one.
- **Per-item letter chords where rows are nameable** (`chords` option): the status picker's `t/w/r/n/h/y` states, per-harness `c/x/o` "new session" rows. Render the letter dim in the row (`PickerModal`'s `itemKeys`) so the chord is discoverable.
- **Sub-affordances get their own letter** (`l n` new section, `! c` custom prompt, `; c` new claude session). The trigger re-press always means "confirm the highlight", never "jump to the special row".
- **Live preview on the bottom pane when it helps** (outputs, sessions) via `previewFocusPatch` from `tui/picker-preview.ts`; pickers without a sensible preview leave the pane alone.
- **`x` kills** where rows represent killable things — DIRECTLY, no confirm: reaching the row already took two deliberate steps (`;`, navigate), and the kill is narrated on the event feed. Only the Shift+F10/F11 shell/diff kills route through `killSessionConfirm` (single-chord openers with no picker in between). Forgetting a dead ghost is likewise immediate — there's nothing to lose.
- **Confirm modals cancel on their opening key.** `handleYesNoKey`'s `extraCancelKeys` carries the opener (`d`, `c`, `e`, `E`, `w`, `!`) so the muscle-memory toggle works on the destructive path too. Openers that aren't a single bare key (Ctrl+R, Shift+F10/F11, Enter) keep just the universal cancels.
- **Hints reflect the chord** — render the trigger-confirm pair in the modal's `hints`; `PickerModal` / `MultiPickerModal` take a `toggleKey` prop that wires this.
- **Unbounded lists scroll, don't clip.** The `Modal` shell clips overflow with no scrollback of its own, so any list that maps user-sized data (actions, sessions, branches, outputs, clean candidates) wraps its rows in `<ScrollableList>` (`tui/panels/scroll-list.tsx`): it fills the modal and scrolls the selected row into view as j/k moves (each row carries a stable `id`, and `selectedId` names the highlighted one). Rows still own horizontal truncation (`wrapMode="none" truncate` inside a `flexGrow`/`overflow="hidden"` box) — vertical scroll, horizontal ellipsis.
- **One scrolling standard.** Every scroll region is a `WtScrollbox` (`tui/scrollbox.tsx`) — themed thumb/track, a reserved one-column gutter so the bar never covers content (row width budgets must count it), no mount flash — and every line-scroll keystroke moves `SCROLL_STEP` (3) rows: the feed chord, the details chord, and overlay j/k via `handleOverlayScrollKey` (which adds PgUp/PgDn half-page, g/G + Home/End edges, and the Ctrl feed-chord aliases; overlays register their box through `useOverlayScroll`, never `focused` — the focused-scrollbox built-in steps 1/5 viewport and would fork the feel). Don't hand-roll a `<scrollbox>` or invent a new step size. Cursor-following is `scrollCursorIntoView` (same module), which keeps `CURSOR_SCROLLOFF` (3) rows of context beyond the selected row — vim's `scrolloff`; the library's bare `scrollChildIntoView` parks the cursor on the viewport edge for the rest of the list.
- **Panes clip, and pane content that can outgrow the pane scrolls.** Every detail body (`panels/details.tsx`, `panels/details/*`) sets `overflow="hidden"` on its bordered box, and any body whose content is user-sized — a worktree's rows and comments, a folded section's members and blocked notes — puts that content in a `WtScrollbox` wired to the shared `scrollRef` (so `Ctrl+J`/`Ctrl+K` page it), with fixed chrome like the key-hint line kept OUTSIDE the scroll region and marked `flexShrink={0}`. Both halves are load-bearing, and each fails differently: without the clip an overflowing pane keeps painting past its own border, over the pane below; without the scroll region the overflow is simply unreachable. And a `<text>` that flexbox squeezed to zero height still draws its line, over whatever now occupies that row, with its spaces transparent — so the symptom is two unrelated lines interleaved character by character, which reads as a corrupted renderer rather than as overflow. Anything directly under a height-constrained column box carries `flexShrink={0}`.
- **OpenTUI focus is off** (`autoFocus: false` at `createCliRenderer`). wt owns its keyboard end to end and has no focusable widgets — every text input is drawn and keyed by hand — so focus buys nothing and costs: a focused renderable installs a GLOBAL keypress handler, and `autoFocus` focuses the first focusable ANCESTOR of whatever gets left-clicked, which is always a scrollbox. One stray click (focusing the terminal window suffices) and every subsequent `j` moved the cursor AND jerked some pane 1/5 of a viewport — modifiers ignored, so the `Ctrl+J`/`Ctrl+K` chords hit it too, often scrolling a pane the key has nothing to do with. It presented as "wt gets weird after it's been open a while", because the trigger was a click long since forgotten. Anything that needs focus in future turns it on for that widget, not globally.
- **Modals size to their content.** The `Modal` shell grows with its children up to the inset-derived height cap — a seven-row picker is a seven-row modal. `fill` opts back into the full fixed frame for content that owns the space (help) — a bare `flexGrow` scrollbox doesn't self-measure and collapses under auto-height, which is also why `ScrollableList`-based pickers work unchanged. Hint chips along the bottom edge wrap BETWEEN hints at narrow widths (`KeyHint` renders each pair as one non-wrapping `<text>` inside a `flexWrap` row) — never through the border.
- **Long prose wraps through `wrapText` (`tui/text.ts`), not `wrapMode="word"`.** opentui's native word wrap lives in the Zig text buffer and has two visible defects: it keeps the whitespace it broke on, so continuation lines start indented by however many spaces the break ate, and it drops the break character when the tail lands exactly at the edge, spending a blank line on it (a phantom empty row under every long status note). `wrapText(text, width, firstWidth?)` pre-splits into lines rendered as `wrapMode="none"` siblings; the optional narrower `firstWidth` is the hanging-indent case (the attention feed's first line shares its row with the time+source prefix). It needs a cell budget, so the caller has to know its pane width — `details.tsx` owns `PANE_CHROME_WIDTH` and passes content width down; the bottom pane spans the terminal and derives its own. Converted: the attention feed, the work-status note (now per labelled section, so `OPS:`/`REVERT:`/`IF WRONG:`/`UNTESTED:` wrap under a hanging indent as a label column) and the post-merge steps. The remaining `wrapMode="word"` sites are short or mixed-span text where the artifacts don't show.
- **Row columns must clip, not shrink.** Two safe shapes: a single `<text wrapMode="none" truncate>` composing columns with spans + `padEnd`, or (for label+value rows like yank's) a `flexShrink={0}` box around the fixed prefix plus a `flexShrink={1} overflow="hidden"` box around the value — the `row-cell.tsx` pattern. What garbles is anything else: bare `<text>` flex siblings shrink under width pressure (columns misalign) or overprint the pane behind (the old yank modal bleed at narrow widths).

When a picker doesn't naturally have a single trigger key (e.g. branchPicker, reached mid-flow), drop the re-press leg and keep Enter/Esc — don't invent a trigger key to satisfy the rule.

## Rendering & input latency

The render loop is **on-demand**: a React commit requests a frame, the frame walks the renderable tree and repaints, and an idle app paints nothing at all. Keeping it that way is a set of invariants, each of which was once violated and measured (idle instances burned ~13% CPU each and j/k queued behind render churn — see the perf skill's notes for the investigation):

- **No OpenTUI Timelines, no `requestAnimationFrame`.** Any playing timeline holds a renderer-wide "live" request: the loop goes continuous (full tree walk + full repaint per tick) and `requestRender()` becomes a no-op, so a keypress commit can't pull a frame forward. All chrome animation rides the shared refcounted ticker in `tui/spinner.tsx` (`useAnimationTick`) — ~10fps, only while an animated component is mounted and visible, one batched commit per tick.
- **Renderable count is a per-commit cost** — every commit's frame walks the whole tree, and scrollbox children pay a layout readback even when culled offscreen. Anything that maps an unbounded buffer renders a window: the events feed (`panels/activity.tsx`) draws a bottom-anchored `TAIL_WINDOW` slice behind an exact-height spacer, expanded ahead of the reader by a slow geometry check and snapped back at the bottom. New unbounded surfaces follow that pattern.
- **App never observes per-event churn.** `useIsFetching` lives in `panels/title-bar.tsx` (a memoized leaf), NEVER in App — it re-renders its component on every fetch start/finish anywhere. The registries (session/shell/harness tails, actions) replace only the touched entry per update, and their hooks subscribe with per-key selector snapshots (`useSessionRun` et al.), so a pane tailing one session doesn't re-render when another streams. The `! l` dev-log overlay follows the same boundary in miniature: its one-second tmux snapshot poll exists only while that modal is mounted, so log changes re-render the overlay leaf and closing it stops the poll. Aggregations that App does need are identity-stabilized: `useActiveActions` returns the previous Set when membership is unchanged, `useOutputs` returns the previous list when membership/order/status are unchanged (timestamps deliberately excluded). `WorktreeList` and `Details` are `React.memo`'d on the back of all this — new props into either must stay identity-stable across unrelated renders.
- **Parsing stays off the render thread.** The claude session-jsonl tailer (`core/harness/claude/tail-worker.ts`), Codex event poller, detailed Codex output tail (`core/harness/codex/tail-worker.ts`), and Codex historical-session discovery (`core/harness/codex/discovery-worker.ts`) do their directory walks, file reads, and JSON parsing in workers; the main thread applies parsed results. Discovery is serialized and abort-aware so rapid cursor movement retains only the current queued destination instead of building an obsolete scan backlog; tail polls allow only one in-flight batch. A new tail-shaped data source follows the same seam.
- **`WT_PERF=1` measures all of it**: the loop-lag probe logs any >20ms sync block, and the input-latency probe logs a p50/p90/max keypress→painted-frame histogram every 60s plus the renderer's live-mode duty cycle — nonzero duty means something re-armed continuous rendering and is a regression. Healthy figures: p50 under ~10ms, live duty 0.

## Work status

`core/work-status.ts` is the pure module behind `wt status` / the `u` picker: the fixed eight-state vocabulary, prefix resolution, urgency ranking, and the two derived overrides in `effectiveWorkState` (a session waiting on input renders as needs-human whatever was asserted; a LANDED row still owing a `verifyAfterMerge` check renders as needs-testing, which is exactly what it now is). `blockedOn` is the one field that changes how a state RENDERS rather than what it says — `isBlockedReady` is the single predicate behind the dot, the banner, `workRecordRank`, the CLI and the automation gate, so a gate hand-written onto a non-`ready` record is inert everywhere instead of honoured by some readers and ignored by others. `WtSlugState.examined` sits beside it and is deliberately NOT part of the record: a work status is what the row's OWNER claims, while a verdict is what an observer concluded, and collapsing the two would lose which is which. `verifyAfterMerge` is its mirror image and the second field that changes how a state renders: dormant before the merge (it must never touch the merge band — merging is the prerequisite, not the thing gated), and after it the row stops sinking to `LANDED_RANK`, takes the marker slot back from the merge glyph, and reads as a `destroyHazard` so no sweep can take the checkout the check still needs. `owesPostMergeVerification` is that single predicate, `rowHasLanded` (app-helpers.ts) is the landed signal it takes, and `verificationOverdue` is the louder tier behind the red dot and the `status.verification_overdue` trigger. It is also the one field a fresh assertion CARRIES rather than drops — a standing obligation about the branch, not a claim inside one assertion — which is why both the CLI's set path and the picker's `carriedVerify` have to agree about it. The picker SETS it too, from a second `ready` row (`ready + verify after merge`, chord `a`) that collects the steps in the footer instead of writing straight away: `statusPickerItems` and `statusTextRecord` in `tui/flows/work-status.ts` are the pure halves, and the split exists because the obligation is a different thing to assert than the state, not a shade of it. That row's pre-filled box is also the one path allowed to DROP the field without `verified`/`dropped` — safe only because it is neither silent nor inferred (the human deletes visible text, and the toast says what was stored). The record itself (`{state, note?, risk?, at, sha?, by?, blockedOn?, verifyAfterMerge?}`) lives in `WtSlugState.work` — so persistence, cross-process propagation (the SQLite WAL watcher), remote transport (`wt ls --json` → `remoteWorktreesQuery`), and TUI freshness were all already wired. The rules that make agent assertions trustworthy (ready needs `--risk`, needs-human needs a note) are enforced in `cli/commands/status.ts`, deliberately NOT in the setter — the TUI picker stays lenient for the human. `by` is stamped there too, from `core/agent-identity.ts` (the `WT_AGENT` reader shared with the fleet-mail sender tag; a leaf module so neither attribution path drags machinery in), and it is what lets a `status.*` automation tell an escalation from an echo of its own write — see [automations.md](automations.md#a-briefing-never-echoes-its-own-audience). Reading it in the setter instead would be the environment trap in reverse: wt usually runs inside a session, so the TUI would attribute the human's `u` picker to whichever agent launched wt. Consumers: `workStatusBadge` (badges.ts) renders the dot, `rowWorkRank`/`sortActiveRows` (useWorktreeRows.ts) drive the `[ui] sort = "status"` ordering, `rows/status.tsx` is the details row, and `useWtStateEvents` narrates observed transitions into the attention feed. **The three free-text fields are rendered gate → note → steps, and that order is load-bearing**: they answer questions at different times (may this merge AT ALL / what a merger needs to know NOW / what whoever holds the row does AFTER it lands), and rendering the dormant one first put a 1896-character field above both, pushing the note, the gate and every definition row below the fold. `tui/work-status-text.ts` recovers the shape both fields are written to — `sanitizeWorkNote` collapses newlines at WRITE time (right: notes reach osascript and terminal titles) but the labels survive, so `splitNoteSections` and `parseVerifySteps` find the seams at RENDER time and every case degrades to the one unstructured block it was before. `parseVerifySteps` only accepts a step run numbered consecutively from 1, because half a parse is worse than none — a stray " 3. " would present a sentence fragment as step 3 of 3. `WorkStatusBlock` then collapses the steps to a marked two-line preview until the check comes due (`V` toggles, `verifyStepsOpenByDefault` is the shared predicate), and `workStatusSuffix` clamps each field to its own budget for the one-line surfaces that render many records at once. Colour discriminates rather than decorates in both: the note's `UNTESTED` label and a `REVERT: no:` value are the only tinted spans, and the steps put the state colour on the header and the step numbers while the prose sits at `fgMid` — fourteen lines at full warn saturation say only "this is long". That hook is the wtstate **narrator**: it diffs the slugs map on every change and emits for both asserted statuses and section moves, because the writer is usually another process (`wt status` / `wt section` in an agent's shell) and a call-site emit would double-log ours while missing theirs. Mutations mark what they're about to write in `state/self-writes.ts` so the diff can tell "the human just pressed a key here" (firehose, or a suppressed toast) from "something else changed their board" (attention feed).

## Logging

`src/core/logger.ts` gives every source three channels: file-only `debug/info/warn/error(msg, ctx?)`; `event.{info,ok,warn,err,dim}(text, opts?)` which fans out to the file *and* the bottom pane's firehose feed (when the TUI runtime has registered a sink); and `attention.{info,ok,warn,err}(text, opts?)` for the curated attention feed — the pane's default view, reserved for things worth interrupting a scan for (work-status transitions, needs-you signals, new PR comments from other people via `usePrCommentEvents`, detached dev-supervisor crashes via `useDevServerEvents`; `event.err` lines surface there too by level). Lazy daily file at `~/.cache/wt/logs/app/wt-YYYY-MM-DD.log`, 14-day retention, cross-process append-safe. `tui/activity-log.ts` is just the in-memory store + `useEvents` hook — emit through `createLogger(...)`.

Writes are an async `appendFile` chain, so **both exit paths drain it**: the TUI in its shutdown sequence, and `main.ts` before its hard `process.exit`. Without the latter a short command (`wt status`, `wt section`) returned and exited with its lines still queued, silently losing the file-only audit trail those commands write specifically to be grepped — and any warning raised during a state read.

**Toasts** (`tui/toast.ts`) are the footer's transient one-liner: a single latest-wins slot, colored by level, auto-expiring. Two producers, per the contract in AGENTS.md: keystroke feedback goes through the flows' `ctx.toast(message, color?, ms?)` (a wrapper over `showToast` — toast-only, never logged), and background code toasts through the logger — `attention.*` emits toast by default (`{toast: false}` opts out), `event.*` emits opt in with `{toast: true}`. The logger side is `setToastSink`, registered by `attachLoggerToasts()` in `tui/runtime.tsx`; CLI runs have no sink, so the flags are inert there.

Per-worktree destroy logs live one level up at `~/.cache/wt/logs/<slug>-*.log`; `wt logs <slug>` tails the latest. Event lines in the daily file are tagged `EVENT` (firehose) or `ATTN` (attention), so `grep ' EVENT \| ATTN '` reconstructs what the pane showed.

**Unhandled errors never touch stdout/stderr while the TUI runs.** `tui/error-store.ts` owns the capture: `installProcessErrorCapture()` (armed in `tui/runtime.tsx` for exactly the renderer's lifetime) replaces Bun's default uncaughtException/unhandledRejection reporters — whose raw multi-line stack print over the alternate screen was the original garbling incident — with a 5-entry in-memory ring, a `log.error` (full stack, file-only), and a `log.event.err` one-liner with `{toast: true}`. `tui/error-boundary.tsx` is the third origin, catching render errors into the same ring (its crash screen replaces the app tree, since a modal can't render there). The error overlay (`panels/error-overlay.tsx` + `modal-keys/errors.ts` + `flows/error-report.ts`, modeled on the perf overlay including the `i` inject flow) auto-pops via `hooks/useErrorOverlay.ts` — queued behind any open modal, acknowledged on dismiss. Deliberate semantics: an uncaughtException keeps the process alive but marks it degraded (banner in the overlay); consecutive identical errors collapse (`×N`) instead of flooding; the renderer's `openConsoleOnError` is disabled so OpenTUI's own error hook can't pop its debug console over the panes; capture detaches right after `renderer.destroy()`, so errors thrown through `runTui()` itself still reach `main.ts`'s top-level catch (and its crash-rollback offer) on plain stderr. `WT_DEBUG_THROW=1|rejection` is the permanent probe hook.

## Stable files

These define contracts; touching them ripples. Read them first:

- `src/core/config.ts` — schema, defaults, validation ([reference](configuration.md)). The user config is recursively overlaid by the nearest `.wt.toml`; arrays replace whole, and `WT_REPO_CONFIG` preserves selection across child processes. Fail-fast loader, one aggregated error. Optional sections (`sst`, `issueTracker`, `devServer`, `naming`) are `null` when absent; `reviewBot` is always present (CodeRabbit preset when `[review_bot]` is omitted), as are `editor` (whose `command: null` selects the built-in Zed path in `core/editor.ts`) and `tmux` (whose `socket` resolves `WT_TMUX_SOCKET` → `[tmux] socket` → `"wt"`, env-first because that half propagates into spawned sessions); `requireSst()` is the typed boundary for SST-only paths. Pure discovery/merge helpers live in `src/core/config-layer.ts`, including `canonicalRepositoryConfig` — the seam that keeps repository identity a property of the REPOSITORY rather than of the caller's cwd. Discovery walks up from the working directory, so it finds a worktree's copy of the repository `.wt.toml` from inside a worktree and nothing at all from a shell outside the repo; `canonicalRepositoryConfig` re-points both cases at `<paths.main_clone>/.wt.toml` before the build, and `build` identifies by `main_clone` anyway whenever the file it was handed sits under `worktree_root`. Everything namespaced follows that one answer — `repoId`, `repoPath`, the state database, the cache root and its whole runtime tree, and the tmux socket — and all of them key on the same predicate (does the repository declare itself with its own `.wt.toml`), never on whether discovery happened to find a file. A `.wt.toml` outside `worktree_root` is still a repository of its own, which is what keeps several repositories isolated.
- `src/tui/rows/types.ts` — the `RowModule` contract; `src/tui/rows/index.ts` — the registry.
- `src/tui/hooks/useWorktreeRows.ts` — per-worktree field aggregator (`FieldState<T>` carries `error`).
- `src/core/diff/` — graceful-degradation diff compactor for the AI pipeline (`parts.ts` parses, `render.ts` transforms per mode, `fit.ts` runs the priority-aware greedy reducer). Cache keys are SHA-256 prefixes of the *unfiltered* diff so filter tweaks don't invalidate prior summaries.
- `src/core/ai.ts` — harness-backed naming pipeline returning `{title, brief, description}` from a line-prefixed response, with a lenient parser; `core/harness/completion.ts` owns the safe one-shot CLI contract.
- `src/core/logger.ts` — see above.

## Workspace prototype

`core/workspace.ts` owns an opt-in outer tmux server per invocation. Its
presentation panes are distinct from the existing persistent-session server.
`main.ts` launches it only with `WT_WORKSPACE=on` in a wide TTY; the child
explorer carries `WT_WORKSPACE_SOCKET` to prevent recursion. Compact rendering
is selected by the explorer's pane width; F9 uses native zoom for the dashboard.
`useWorkspaceHelp` opens a native tmux popup, leaving pane geometry and zoom
untouched. Its `_help-popup` process renders only the shared `HelpOverlay` and
uses the shared help keyboard handler; it starts no fleet queries or session
runtime. The explorer suppresses its own help overlay while the popup is open.
`useWorkspaceActions` does the same for the row `!` menu and its edit/argument
screens. A private temporary Unix socket sends view snapshots to `_action-popup`
and relays key/paste events back to the explorer's existing handlers. The
explorer owns the captured target, availability checks, and action execution;
the popup has no fleet queries or action runtime. Closing either side disposes
the connection and popup; input received after the action modal closes is ignored.
Popup modals fill their terminal width without the normal modal width cap; both
popup renderers paint the theme background so no bare terminal gutters show.
The main TUI renderer uses the same background for any space outside its layout.

Session flows route workspace targets to a thin `_workspace-host` command.
The fullscreen entry and local content host share `navigateWorktreeSession`;
remote content uses the existing `_session` SSH transport. Target activation
is serialized and stale requests cannot take focus. Pane IDs survive index
changes; process ownership is checked before replacement. Host HUP/TERM
interrupts its scoped attach client, not the underlying session. Workspace
identity is removed from persistent-session environments. The prototype's
remaining acceptance work is documented in [tui.md](tui.md#experimental-sticky-explorer-issue-18).
