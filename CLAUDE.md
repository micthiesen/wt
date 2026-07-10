<!--
Maintain this file proactively. When a session surfaces a new trap, a
shifted contract, or a principle that no longer applies, update this
file without being asked. The bar is "non-obvious and load-bearing" —
add sparingly, prune freely. Traps not maps; maps live in docs/.
-->

# wt — for Claude

Personal git-worktree TUI. Single developer, single machine, OSS-published. Bun + React + OpenTUI on top of TanStack Query.

## Workflow

Commit and push directly to `main`. Don't create feature branches, don't open PRs, don't suggest either — unless the user explicitly asks. This is a single-developer repo and the feature-branch dance is pure overhead here. When in doubt, stay on `main`.

## Docs are dual-purpose — keep them current

`docs/` serves users AND agents; this file holds only the rules and traps. When a change touches any of these surfaces, update the matching doc in the same commit:

- `docs/architecture.md` — the internals map: layers, composition root, module layout, freshness-trigger inventory, modal UX rules, logging, stable files. **Read it before structural work, picker/modal work, or adding a state source.**
- `docs/configuration.md` — mirrors the `src/core/config.ts` schema. Any schema/default change updates it.
- `docs/tui.md` — keymap + panes. Any keybinding change updates it (and `src/tui/panels/help.tsx`, the in-app source of truth).
- `docs/cli.md` — subcommands + flags. Any CLI change updates it.
- `docs/automations.md`, `docs/github-events.md`, `docs/stacked-prs.md` — per-feature semantics.
- `README.md` — concise front door; it links out rather than duplicating. Keep it short.

## Architecture (rules; map in docs/architecture.md)

- Three layers — sources (`src/state/queries/`), rows (`src/tui/rows/`), driver (`src/tui/panels/details.tsx`). Rows are pure-presentational; fetching stays in sources. Don't cross the boundaries.
- `src/tui/app.tsx` is the composition root and only routes; new flow logic goes in `src/tui/flows/`, new modal keys in `src/tui/modal-keys/`, not back into app.tsx. Keyboard dispatch order (modal → footer input → removed view → `h` → normal) and the handler order inside `normal-keys.ts` are load-bearing.
- Big core modules are directories behind same-named flat barrels (`core/github.ts` → `core/github/`, …); only names re-exported by the barrel are public, and importers use the flat path. Per-harness code lives under `core/harness/<harness>/` behind the `Harness` interface.
- `src/core/config.ts` defines the config contract (fail-fast loader, optional sections `null` when absent, `requireSst()` for SST-only paths). Read the stable-files list in docs/architecture.md before touching contract files.

## Working principles

- **Source fetches stay batched.** The github source is one GraphQL round-trip aliasing every per-worktree PR field plus the repo `mergeQueue` block. Never split it into per-row fetches; rate limits and latency are real. New PR fields go into `PR_FRAGMENT` in `core/github/fetch.ts` rather than getting their own query.
- **AI summary is hash-keyed.** `aiSummaryQuery`'s queryKey is `["aiSummary", <hash>]`; equivalent diffs across rebases/amends/renames hit the same entry. "Previous summary stays visible while a new hash fetches" comes from `placeholderData: keepPreviousData` at the observer — no mismatch effect, no force-regen flag; the hash flipping IS the trigger. `refreshAiSummary` (`t`) refetches the diff context and `removeQueries` the entry for the resulting hash.
- **Pluginify reactively.** Built-ins now, plugins only when there's a second concrete implementation that justifies the seam. Don't pre-design for "what if someone wants Jira."
- **No client-app defaults in code.** `paths.main_clone`, `paths.worktree_root`, `branch.prefix` are required and the loader refuses to start without them. New required fields go through `Errors.reqStr`; add a derivation when one's natural (see `stage.prefix` defaulting from `branch.prefix`).
- **Errors render verbatim, gated on retries-exhausted.** `firstError` in `details.tsx` already handles the gate (`error && !isFetching`) — don't duplicate it elsewhere. The same error showing on every row that depends on a broken source is intentional.
- **Convention over configuration for the niche stuff.** `branch.id_pattern` exists but most users will never set it; the default matches Linear/Jira/Shortcut conventions.
- **Stacks are inferred, never stored.** The per-slug fork-base record (`baseBranch` + `baseSha` in wtstate) is the ONLY stack state; grouping, diff bases, and the restack engine all derive from it (`core/stack-layout.ts` infers, `core/stack-ops/` replays). Don't add stack registries, manifests, or membership caches — and never drop a `baseSha` when rewriting a record's branch: it's the squash-safe replay anchor (see docs/stacked-prs.md).
- **Mutating GitHub state? Invalidate `["github"]`, not the worktree.** Write-path `gh` calls must trigger `refreshGithub()` from `state/hooks.ts`. `invalidateWorktree(slug)` looks plausible but the github query is keyed by branch list, not slug — it silently misses.
- **Automations are level + ledger, never edge-triggered.** The `[[automations]]` engine (`core/automations.ts` ledger, `tui/automation-rules.ts` pure evaluation, `tui/hooks/useAutomations.ts` queue + dispatch) re-derives conditions from row state every pass; once-only comes from persistent fire keys keyed on head SHA. Hard rules: `markFiresDispatched` runs synchronously before any await in a dispatch; PR-driven conditions require `githubFresh` plus `pr.headRefOid` (persisted-cache data must never fire); failures mark delivered and never retry (new SHA = the retry); the per-(rule, target) breaker trips after `BREAKER_LIMIT` consecutive no-clear dispatches and resets only when the condition is observed false. New triggers: add to `AutomationTrigger` in config.ts + a case in `automation-rules.ts` with a documented fire key. Runs dispatch through the same paths keystrokes use, never bespoke ones. Semantics doc: docs/automations.md.
- **Freshness is push-based; `r` is a backstop, not the mechanism.** Every external state source has an event trigger that invalidates the matching query — the full trigger inventory is in docs/architecture.md#freshness-model. When adding a state source or mutation path, wire a watcher or an explicit invalidation at the call site rather than shortening a staleTime or telling the user to press `r`; staleTimes only bound how wrong things can be when a trigger is missed.
- **Pickers follow the shared modal UX rules** (trigger-key re-press confirms, Enter confirms, Esc/q/Ctrl+C cancel, 1–9 quick-pick, sub-affordances get their own letter). The full checklist is docs/architecture.md#modal-ux-rules — hold to it when adding or modifying any picker; deviations fragment the UX.

## Logging & debugging

- Daily app log: `~/.cache/wt/logs/app/wt-YYYY-MM-DD.log` — read it when something looked wrong; event lines are tagged `EVENT` (`grep ' EVENT '` shows what the activity pane showed). Destroy logs: `~/.cache/wt/logs/<slug>-*.log` via `wt logs <slug>`.
- Adding a log call: `createLogger(source)` at module top (or per-call for dynamic slugs). `log.event.X` when the user should see it in the activity pane; `log.debug/info/warn/error` for file-only. `log.error` does **not** auto-promote to the pane.

## Tooling

- Runtime is **Bun**. No node, no pnpm. `bun install`, `bun src/main.ts`.
- Typecheck: `bun run typecheck` (just `tsc --noEmit`).
- Tests are opt-in, not comprehensive: `bun test`. The restack anchor tests (`src/core/stack-ops.test.ts`) pin the squash-safe cut-point logic against real git repos — add cases there when touching `resolveAnchor`. Most other modules have no tests; smoke via the CLI.
- The TUI takes over the terminal — smoke-test refactors via the CLI subcommands (`bun src/main.ts ls`) or check imports with `bun -e 'import("./src/path.tsx")'`.

## Traps

- **The list panel (`src/tui/panels/list.tsx`) is NOT row-driven.** Different layout (single line of glyphs, no labels), intentionally not wired through `rows/`. Don't try to unify them.
- **Config loads once at module init.** No hot reload — editing the TOML requires restarting `wt`.
- **`Bun.TOML.parse` is built in.** No external TOML lib needed; don't add one.
- **macOS-only utilities** (`open`, `pbcopy`, launchd) are assumed. Anything that shells out to them stays guarded by the macOS assumption noted in the README.
