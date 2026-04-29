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

## Working principles

- **Source fetches stay batched.** The github source is one GraphQL round-trip aliasing PR + checks + review + merge queue per worktree. Never split it into per-row fetches; rate limits and latency are real.
- **AI summary is content-addressed, not slug-addressed.** `aiSummaryQuery`'s key is `["aiSummary", <hash>]` so equivalent diffs share a cache entry across rebases, amends, and branch renames. `slug` is passed in for log attribution only — never bake it into the cache key, or you re-trigger the LLM every time someone renames a branch.
- **Pluginify reactively.** Built-ins now, plugins only when there's a second concrete implementation that justifies the seam. Don't pre-design for "what if someone wants Jira."
- **No client-app defaults in code.** `paths.main_clone`, `paths.worktree_root`, `branch.prefix` are required and the loader refuses to start without them. New required fields go through `Errors.reqStr`; add a derivation when one's natural (see `stage.prefix` defaulting from `branch.prefix`).
- **Errors render verbatim, gated on retries-exhausted.** `firstError` in `details.tsx` already handles the gate (`error && !isFetching`) — don't duplicate it elsewhere. The same error showing on every row that depends on a broken source is intentional.
- **Convention over configuration for the niche stuff.** `branch.id_pattern` exists but most users will never set it; the default matches Linear/Jira/Shortcut conventions.

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
