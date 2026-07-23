# Architecture

Internals map for contributors and coding agents. Bun + React + [OpenTUI](https://github.com/sst/opentui) on top of TanStack Query. The companion rules file for agents is [`CLAUDE.md`](../CLAUDE.md); this page is the *map*, that one is the *rules*.

## The three layers

The TUI is split into three layers; respect the boundaries:

- **Sources** — `src/state/queries/` (per-source files behind the `src/state/queries.ts` barrel), `src/state/hooks.ts`, `src/tui/hooks/useWorktreeRows.ts`. They own fetching, batching, and caching via TanStack Query. Small fixed set (github, git, sst, claude, linear-derived, ai); not user-pluggable.
- **Rows** — `src/tui/rows/*.tsx`. Pure-presentational modules declaring `{id, label, sources, render, visible?}`. Multiple rows can read from the same source; the source still fetches once. `src/tui/rows/index.ts` is the registry; `[ui].rows` in the user config selects + orders them, and a row hides itself when its integration isn't configured.
- **Driver** — `src/tui/panels/details.tsx`. Iterates the configured row list, computes the trailing staleness glyph, and renders inline errors verbatim once retries are exhausted. Also owns the AI title/description band above and below the row stack — pane-level chrome, not a row.

The list panel (`src/tui/panels/list.tsx`) is deliberately **not** row-driven — different layout (one line of glyphs, no labels). Don't try to unify them.

## Composition root

`src/tui/app.tsx` wires everything: state declarations, hook wiring, per-render flow factories, the ctx objects key handlers destructure, and the layout JSX. The pieces:

- **Keyboard** — `src/tui/keyboard/` (`global-keys.ts`, `footer-input-keys.ts`, `removed-view-keys.ts`, `normal-keys.ts`) plus `src/tui/modal-keys/` (one file per modal family; `index.ts` is the dispatcher). The `useKeyboard` callback in app.tsx only routes, in load-bearing order: modal → footer input → removed view → `h` toggle → normal mode. Handler-check order *inside* `normal-keys.ts` is also load-bearing (see its header comment).
- **Flows** — `src/tui/flows/` (`destroy.ts`, `sessions.ts`, `github-pr.ts`, `sections.ts`, `base.ts`, `reviewers.ts`, `new-worktree.ts`, `action-picker.ts` — per-render factories over a context object) and `src/tui/hooks/useActionDispatch.ts` (action launch + completion subscriber). New flow logic goes in a flows module, not back into app.tsx.
- **Modal overlays** — `src/tui/modal-host.tsx` (`PreFooterModals` mount before the Footer, `PostFooterModals` after; render order is paint order). The modal union lives in `src/tui/modal-state.ts`; `modal.tsx` is the shared chrome component.
- Pure helpers in `src/tui/app-helpers.ts`; title-bar badges in `src/tui/usage-badge.tsx`.

## Module layout conventions

The big core modules are directories behind a same-named flat barrel: `core/github.ts` → `core/github/`, `core/wtstate.ts` → `core/wtstate/`, `core/stack-ops.ts` → `core/stack-ops/`, `core/actions.ts` → `core/actions/`, `core/tmux.ts` → `core/tmux/`, `core/hub.ts` → `core/hub/`, `state/queries.ts` → `state/queries/`. The barrel re-exports the module's public surface with explicit named re-exports — importers keep using the flat path; only names in the barrel are public. (`tui/modal-keys/` is a plain directory — its single consumer imports `index.ts` directly.)

Per-harness code (claude/codex/opencode session discovery, naming, events, usage, tails) lives under `core/harness/<harness>/` behind the generic `Harness` interface (`core/harness/types.ts`); `core/harness/status.ts` is the shared `DerivedState` vocabulary.

Worktree **backends** follow the same shape: `core/backend.ts` → `core/backend/` behind the narrow `WorktreeBackend` interface (`create` / `remove` — the only two filesystem mutation points, extracted from `lifecycle.ts`). Two built-ins: `git-worktree` (linked worktrees, one shared object db) and `rift` (copy-on-write clones). Everything else wt does to a worktree (fork-base record, `.env` copy, stage pin, upstream, status) stays backend-agnostic in `lifecycle.ts` / `worktree.ts`. `getBackend(kind)` picks the create backend from config; `getBackendForPath(path)` derives the owning backend from disk (a `.rift` marker) so removal is correct after a config flip. This is the LOCAL-materialization axis, orthogonal to any remote (SSH-host) axis. See [backends.md](backends.md).

## Freshness model

Freshness is **push-based**; the `r` keybind is a backstop, not the mechanism. Every external state source has an event trigger that invalidates the matching query:

| trigger | invalidates |
|---|---|
| `.git/refs/` watcher (commits, fetches, pushes) | github + per-worktree fields + wtState |
| `.git/worktrees/` watcher (worktree add/remove) | worktree list |
| worktree-root watcher (subdir add/remove) | worktree list — catches `rift` checkouts, which are independent clones that never touch `.git/worktrees/`; harmlessly redundant for git worktrees |
| `.git/worktrees/<slug>/rebase-{merge,apply}` watcher (hand/`/restack` rebase starts or ends) | that slug's conflict probe (the mid-rebase glyph) |
| per-worktree dir watchers | edits → dirty; `.sst/` writes → deploy |
| `~/.cache/wt/state.json` + `archive.json` watcher | cross-process fork-base / section / archive writes |
| `~/.cache/wt/locks/` watcher | per-slug busy state from any process (create/destroy, and every chain member during a restack — the restack glyph rides on this); a release also fans out a per-slug field refresh (`useLockReleasedInvalidator`) **and refreshes the worktree list** — the reliable "a create/destroy just finished" signal, so a new (esp. `rift`) row surfaces immediately instead of waiting on the interval (a rift `.rift` marker is written inside the new dir, after the worktree-root watcher already fired on the bare dir) |
| github-events webhook marker | github + a staleTime-gated `git fetch origin` |
| 3-minute `fetch origin` interval | backstop for remote drift |
| claude-registry fs.watch, session-tail triggers (`gh pr …` / `git push` inside a session) | sessions / github |
| action `affects` tags on completion | the declared domains |
| hub session entry (`switchRight` retargets) + the on-screen re-stamp while a shown session streams output (`useHubController`) | `~/.cache/wt/task-focus.json` — the slug's last-focused stamp, which is what flips a task out of the `review-output` bucket; push-based, no polling |

When adding a new state source or mutation path, wire one of these (or an explicit invalidation at the call site) rather than shortening a staleTime — staleTimes only bound how wrong things can be when a trigger is missed. Watchers live in `src/core/repo-watch.ts` and are wired in `src/tui/runtime.tsx` through a 50ms-coalescing invalidation scheduler.

Two related invariants:

- The github source is **one GraphQL round-trip** aliasing every per-worktree PR field plus the repo merge-queue block. New PR fields go into `PR_FRAGMENT` in `core/github/fetch.ts`, never a separate query.
- Anything that *mutates* GitHub state must invalidate `["github"]` (via `refreshGithub()` in `state/hooks.ts`), not the worktree — the github query is keyed by branch list, not slug.

## Remote execution

The optional `[remote]` host owns its clone, worktree paths, locks, and tmux
processes, while the Mac owns the single visible TUI. `remoteWorktreesQuery`
polls the host's `wt ls --json` and merges those summaries into the local
Inbox; remote filesystem paths are never accessed as if they were local.
The query's successful inventory is persisted for offline startup and retained
across refetch failures. SSH failure changes host health only: rows render a
warning and session keys are disabled until a later poll succeeds.

`core/remote.ts` drives SSH, while `core/remote-protocol.ts` base64url-encodes
the complete argv into a single shell-safe token. The remote `_remote` CLI
entrypoint decodes that token and re-enters normal dispatch, avoiding any
dependency on remote login-shell quoting.

`Ctrl+N` forwards `wt new` and refreshes the remote-row query when creation
finishes. F10/F11/F12 on a remote row use the hidden `_session` entrypoint;
Cachy runs that one worktree's tmux session while `renderer-handoff.ts`
suspends the Mac renderer. Detaching returns to the same Mac Inbox.
`d` forwards the normal `wt rm` command after confirmation, preserving the
remote installation's lock and dirty-work safeguards while explicitly leaving
any SST stage intact.

Hub mode can't hand off the terminal, so it bridges the same `_session`
SSH command through a local **wrapper session** on the inner tmux server
(`core/tmux/remote-wrapper.ts`, reserved names `wt-remote~<slug>~<target>`
excluded from classification and the orphan reaper) and retargets the right
pane at it; wrapper death (SSH drop) is observed by the hub's liveness watch
through the same `tmuxSessionsQuery` every other session kind uses. See
[hub.md](hub.md#remote-worktrees).

## Hub mode

An opt-in second UI (`[ui] mode = "hub"` / `wt hub`) layered on top of the same
worktree pipeline rather than a fork of it — see [hub.md](hub.md) for the full
picture (tmux topology, bucket precedence, keymap). Two things worth knowing
structurally:

- `core/hub/` (behind the `core/hub.ts` barrel, alongside `core/tmux/`) owns
  the outer `wt-hub` tmux server: layout (`layout.ts`), generated config
  (`config.ts`), and the control surface the TUI drives it with —
  `switchRight` retargeting the right pane, `focusLeft`/`focusRight`,
  `killHub` (`control.ts`).
- The task inbox gets its own pass at the sources/rows/driver split: `core/task-state.ts` is the pure bucket-precedence source (no queries, no fs — fully unit-tested), `src/tui/hooks/useTaskRows.ts` is the rows-equivalent glue that turns the existing worktree-row pipeline plus review-request PRs into sorted `TaskItem[]`, and `src/tui/panels/tasks.tsx` (`TaskList`) is the driver — purely presentational, same badge/glyph machinery as `panels/list.tsx`.

## Modal UX rules

Every list-picker modal follows the same shape so muscle memory carries across pickers. Hold to these when adding or modifying a picker:

- **Trigger-key re-press confirms.** Whatever single key opens the picker (`l`, `;`, `'`, `!`, `v`, `b`) also commits the highlighted row when pressed again (`l l`, `; ;`, `' '`, `! !`, `v v`).
- **Enter still works** — the chord is the cheap path, Enter the discoverable one.
- **Esc / q / Ctrl+C cancel.** Universal, no exceptions.
- **j/k or arrows move.** Nothing fancier; `g`/`G` aren't bound inside pickers.
- **1–9 quick-pick** when the list shows ≤9 items; out-of-range digits are ignored.
- **Sub-affordances get their own letter** (`l n` new section, `! c` custom prompt, `; c` new claude session). The trigger re-press always means "confirm the highlight", never "jump to the special row".
- **Live preview on the bottom pane when it helps** (outputs, sessions) via `previewFocusPatch` from `tui/picker-preview.ts`; pickers without a sensible preview leave the pane alone.
- **`x` kills** where rows represent killable things, without an extra confirm.
- **Hints reflect the chord** — render the trigger-confirm pair in the modal's `hints`; `PickerModal` / `MultiPickerModal` take a `toggleKey` prop that wires this.
- **Unbounded lists scroll, don't clip.** The `Modal` shell clips overflow with no scrollback of its own, so any list that maps user-sized data (actions, sessions, branches, outputs, clean candidates) wraps its rows in `<ScrollableList>` (`tui/panels/scroll-list.tsx`): it fills the modal, suppresses the mount scrollbar flash, and scrolls the selected row into view as j/k moves (each row carries a stable `id`, and `selectedId` names the highlighted one). Rows still own horizontal truncation (`wrapMode="none" truncate` inside a `flexGrow`/`overflow="hidden"` box) — vertical scroll, horizontal ellipsis.

When a picker doesn't naturally have a single trigger key (e.g. branchPicker, reached mid-flow), drop the re-press leg and keep Enter/Esc — don't invent a trigger key to satisfy the rule.

## Logging

`src/core/logger.ts` gives every source two channels: file-only `debug/info/warn/error(msg, ctx?)`, and `event.{info,ok,warn,err,dim}(text)` which fans out to the file *and* the activity pane (when the TUI runtime has registered a sink). Lazy daily file at `~/.cache/wt/logs/app/wt-YYYY-MM-DD.log`, 14-day retention, cross-process append-safe. `tui/activity-log.ts` is just the in-memory store + `useEvents` hook — emit through `createLogger(...)`.

Per-worktree destroy logs live one level up at `~/.cache/wt/logs/<slug>-*.log`; `wt logs <slug>` tails the latest. Event lines in the daily file are tagged `EVENT`, so `grep ' EVENT '` reconstructs what the activity pane showed.

## Stable files

These define contracts; touching them ripples. Read them first:

- `src/core/config.ts` — schema, defaults, validation ([reference](configuration.md)). Fail-fast loader, one aggregated error. Optional sections (`sst`, `linear`, `ai`) are `null` when absent; `requireSst()` is the typed boundary for SST-only paths.
- `src/tui/rows/types.ts` — the `RowModule` contract; `src/tui/rows/index.ts` — the registry.
- `src/tui/hooks/useWorktreeRows.ts` — per-worktree field aggregator (`FieldState<T>` carries `error`).
- `src/core/diff/` — graceful-degradation diff compactor for the AI pipeline (`parts.ts` parses, `render.ts` transforms per mode, `fit.ts` runs the priority-aware greedy reducer). Cache keys are SHA-256 prefixes of the *unfiltered* diff so filter tweaks don't invalidate prior summaries.
- `src/core/ai.ts` — OpenAI-compatible / Gemini client returning `{title, brief, description}` from a line-prefixed response, with a lenient parser.
- `src/core/logger.ts` — see above.
