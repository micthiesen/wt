<!--
Maintain this file proactively. When a session surfaces a new trap, a
shifted contract, or a principle that no longer applies, update this
file without being asked. The bar is "non-obvious and load-bearing" —
add sparingly, prune freely. Traps not maps; if something would rot,
leave it out.
-->

# wt — for Claude

Personal git-worktree TUI. Single developer, single machine, OSS-published. Bun + React + OpenTUI on top of TanStack Query.

## Architecture

The TUI is split into three layers; respect the boundaries:

- **Sources** — `src/state/queries.ts`, `src/state/hooks.ts`, `src/tui/hooks/useWorktreeRows.ts`. They own fetching, batching, caching via TanStack Query. Small fixed set (github, git, sst, claude, linear-derived, ai); not user-pluggable.
- **Rows** — `src/tui/rows/*.tsx`. Pure-presentational modules declaring `{id, label, sources, render, visible?}`. Multiple rows can read from the same source; the source still fetches once.
- **Driver** — `src/tui/panels/details.tsx`. Iterates the configured row list, computes the trailing staleness glyph, and renders the inline error message verbatim once retries are exhausted. Also owns the AI title/description band rendered above and below the row stack — the AI pipeline lives here, not in the row registry, because it's pane-level chrome rather than a row.

User config in `~/.config/wt/config.toml` selects + orders rows. A row hides itself automatically when its source isn't configured (e.g. the `linear` row hides when `[issue_tracker.linear]` is absent).

## Stable files

These define contracts. Touching them ripples; read them first.

- `src/core/config.ts` — schema, defaults, validation. Loader fails fast at startup with one aggregated error message. Optional sections (`sst`, `linear`, `ai`) are `null` when absent; `requireSst()` is the typed boundary for SST-only code paths.
- `src/tui/rows/types.ts` — `RowModule` contract.
- `src/tui/rows/index.ts` — registry. Adding a row: write the file, append to `REGISTRY`, optionally add to the default `ui.rows` order in `config.ts`.
- `src/tui/hooks/useWorktreeRows.ts` — per-worktree field aggregator. `FieldState<T>` carries `error`; plumb it through `toFieldState` when adding a field.
- `src/core/diff/` — graceful-degradation diff compactor. `parts.ts` parses, `render.ts` transforms per mode, `fit.ts` runs the priority-aware greedy reducer, `index.ts` is the entry. Modes (`full → tight → hunks → dropped`) and priority tiers are the two knobs worth touching here. Cache keys are SHA-256 prefixes of the *unfiltered* diff so filter tweaks don't invalidate prior summaries.
- `src/core/ai.ts` — LM Studio / OpenAI-compatible client. One call returns `{title, description}` parsed from a `TITLE: …\nDESCRIPTION: …` line-prefixed response. Parser is lenient — falls back to "everything is the description" when the model ignores structure.
- `src/core/logger.ts` — structured logger. Two channels per source: file-only `debug/info/warn/error(msg, ctx?)` and `event.{info,ok,warn,err,dim}(text)` which fans out to file *and* the activity pane (when the TUI runtime has registered a sink). Lazy daily file at `<config.paths.appLogDir>/wt-YYYY-MM-DD.log`, 14-day retention, cross-process append-safe. `tui/events.ts` is now just the in-memory store + `useEvents` hook — emit through `createLogger(...)` instead.

## Working principles

- **Source fetches stay batched.** The github source is one GraphQL round-trip aliasing every per-worktree PR field (state, checks, reviews, requested + suggested reviewers, auto-merge, merge-queue) plus the repo-level merge queue. Never split it into per-row fetches; rate limits and latency are real. New PR fields go into `PR_FRAGMENT` in `core/github.ts` rather than getting their own query.
- **AI summary is dual-keyed: slug primary + content-addressed memo.** `aiSummaryQuery`'s queryKey is `["aiSummary", <slug>]`, value is `{hash, title, brief, description}`. Slug-keying is deliberate — observers keep showing the previous summary while a refetch is in flight after a diff change, instead of the brief vanishing because the new hash has no entry yet. The queryFn checks `["aiSummaryMemo", <hash>]` before calling LM Studio and writes there on success, so equivalent diffs across rebases / amends / branch renames still reuse the prior result. Hash-mismatch invalidation lives in the consumer hooks (`useWorktreeRows`, `details.tsx`) — react-query won't re-run the queryFn just because its closure changed; it needs an explicit `invalidateQueries({ queryKey: qk.aiSummary(slug) })`. `refreshAiSummary` (manual regen) sets a one-shot `aiSummaryForceRegen` flag so the next queryFn run skips the memo lookup entirely. The flag is required because the diffContext refetch inside `refreshAiSummary` can drive the mismatch effect to fire `invalidateQueries(aiSummary)` ahead of the explicit one, and that racing run would otherwise hit the still-present memo. The flag is consumed on read (`Set.delete` returns true iff present) so a follow-up refetch from `isInvalidated` bookkeeping reuses the just-written memo and doesn't burn a second LM call. *Don't* `removeQueries(aiSummaryMemo)` from the regen path — earlier versions did, but with the flag in place that remove is at best redundant and at worst harmful (delete-after-write race when the LM call completes faster than the post-await sync block).
- **Pluginify reactively.** Built-ins now, plugins only when there's a second concrete implementation that justifies the seam. Don't pre-design for "what if someone wants Jira."
- **No client-app defaults in code.** `paths.main_clone`, `paths.worktree_root`, `branch.prefix` are required and the loader refuses to start without them. New required fields go through `Errors.reqStr`; add a derivation when one's natural (see `stage.prefix` defaulting from `branch.prefix`).
- **Errors render verbatim, gated on retries-exhausted.** `firstError` in `details.tsx` already handles the gate (`error && !isFetching`) — don't duplicate it elsewhere. The same error showing on every row that depends on a broken source is intentional.
- **Convention over configuration for the niche stuff.** `branch.id_pattern` exists but most users will never set it; the default matches Linear/Jira/Shortcut conventions.
- **Mutating GitHub state? Invalidate `["github"]`, not the worktree.** TUI actions that hit `gh` for a write (auto-merge arm/disable, mark-ready, edit reviewers, …) must call `refreshGithub()` from `state/hooks.ts` so the badge flips immediately. `invalidateWorktree(slug)` looks plausible but the github query is keyed by branch list, not slug — it'll silently miss and the user will see stale state until the slow staleTime expires.

## Logging & debugging

- **Daily app log**: `~/.cache/wt/logs/app/wt-YYYY-MM-DD.log`. Every `createLogger(source)` call writes here, regardless of TUI/CLI. Plaintext header (`<iso-ts> <LEVEL> <source>`) with structured ctx as a trailing JSON blob. Read it when something looked wrong: errors thrown deep in queries, stack traces from caught exceptions, or to recover what the activity pane scrolled past (event lines are tagged `EVENT`, so `grep ' EVENT '` shows exactly what the user saw).
- **Per-worktree destroy logs** still live one level up at `~/.cache/wt/logs/<slug>-*.log` (separate from the daily app log) and are surfaced via `wt logs <slug>`.
- **Adding a log call**: `const log = createLogger("[some-source]")` at module top for static sources, or `createLogger(slug)` per call for dynamic ones. Use `log.event.X` when the user should see it in the activity pane; use `log.debug/info/warn/error` for file-only diagnostics. The "errors render verbatim, gated on retries-exhausted" rule still applies — `log.error` does **not** auto-promote to the pane.

## Tooling

- Runtime is **Bun**. No node, no pnpm. `bun install`, `bun src/main.ts`.
- Typecheck: `bun run typecheck` (just `tsc --noEmit`).
- No test suite yet.
- The TUI takes over the terminal — smoke-test refactors via the CLI subcommands (`bun src/main.ts ls`) or check imports with `bun -e 'import("./src/path.tsx")'`.

## Traps

- **The list panel (`src/tui/panels/list.tsx`) is NOT row-driven.** Different layout (single line of glyphs, no labels), intentionally not wired through `rows/`. Don't try to unify them.
- **Config loads once at module init.** No hot reload — editing the TOML requires restarting `wt`.
- **`Bun.TOML.parse` is built in.** No external TOML lib needed; don't add one.
- **macOS-only utilities** (`open`, `pbcopy`) are assumed. Anything that shells out to them stays guarded by the macOS assumption noted in the README.
