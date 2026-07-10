<!--
Maintain this file proactively. When a session surfaces a new trap, a
shifted contract, or a principle that no longer applies, update this
file without being asked. The bar is "non-obvious and load-bearing" —
add sparingly, prune freely. Traps not maps; if something would rot,
leave it out.
-->

# wt — for Claude

Personal git-worktree TUI. Single developer, single machine, OSS-published. Bun + React + OpenTUI on top of TanStack Query.

## Workflow

Commit and push directly to `main`. Don't create feature branches, don't open PRs, don't suggest either — unless the user explicitly asks. This is a single-developer repo and the feature-branch dance is pure overhead here. When in doubt, stay on `main`.

## Architecture

The TUI is split into three layers; respect the boundaries:

- **Sources** — `src/state/queries/` (per-source files behind the `src/state/queries.ts` barrel), `src/state/hooks.ts`, `src/tui/hooks/useWorktreeRows.ts`. They own fetching, batching, caching via TanStack Query. Small fixed set (github, git, sst, claude, linear-derived, ai); not user-pluggable.
- **Rows** — `src/tui/rows/*.tsx`. Pure-presentational modules declaring `{id, label, sources, render, visible?}`. Multiple rows can read from the same source; the source still fetches once.
- **Driver** — `src/tui/panels/details.tsx`. Iterates the configured row list, computes the trailing staleness glyph, and renders the inline error message verbatim once retries are exhausted. Also owns the AI title/description band rendered above and below the row stack — the AI pipeline lives here, not in the row registry, because it's pane-level chrome rather than a row.

`src/tui/app.tsx` is the composition root: state declarations, hook wiring, per-render flow factories, the ctx objects the key handlers destructure, and the layout JSX. The pieces it wires:
- **Keyboard** — `src/tui/keyboard/` (`global-keys.ts`, `footer-input-keys.ts`, `removed-view-keys.ts`, `normal-keys.ts`) plus `src/tui/modal-keys/` (one file per modal family; `index.ts` is the dispatcher). app.tsx's `useKeyboard` callback only routes, in load-bearing order: modal → footer input → removed view → `h` toggle → normal mode. Handler-check order *inside* `normal-keys.ts` is also load-bearing (see its header comment); case pairs on one physical key (`g`/`G`, `r`/`R`, `n`/`N`, `e`/`E`, `o`/`O`) disambiguate on `k.sequence`, not the `isPlainLetter`/`isShiftedLetter` helpers — preserve each binding's existing style.
- **Flows** — `src/tui/flows/` (`destroy.ts`, `sessions.ts`, `github-pr.ts`, `sections.ts`, `base.ts`, `reviewers.ts`, `new-worktree.ts`, `action-picker.ts` — per-render factories over a context object) and `src/tui/hooks/useActionDispatch.ts` (action launch + completion subscriber). New flow logic goes in a flows module, not back into app.tsx.
- **Modal overlays** — `src/tui/modal-host.tsx` (`PreFooterModals` mount before the Footer, `PostFooterModals` after; render order is paint order — don't shuffle kinds between them). The modal union stays in `src/tui/modal-state.ts` (`modal.tsx` is the shared modal chrome component).
- Pure helpers in `src/tui/app-helpers.ts`; title-bar badges in `src/tui/usage-badge.tsx`.

User config in `~/.config/wt/config.toml` selects + orders rows. A row hides itself automatically when its source isn't configured (e.g. the `linear` row hides when `[issue_tracker.linear]` is absent).

## Module layout conventions

The big core modules are directories behind a same-named barrel: `core/github.ts` → `core/github/`, `core/wtstate.ts` → `core/wtstate/`, `core/stack-ops.ts` → `core/stack-ops/`, `core/actions.ts` → `core/actions/`, `core/tmux.ts` → `core/tmux/`, `state/queries.ts` → `state/queries/`. The barrel re-exports the module's public surface with explicit named re-exports — importers keep using the flat path; only names in the barrel are public. (`tui/modal-keys/` and `cli/commands/stack/` are plain directories — their single consumers import `index.ts` directly.) Per-harness code (claude/codex/opencode session discovery, naming, events, usage, tails) lives under `core/harness/<harness>/` behind the generic `Harness` interface (`core/harness/types.ts`); `core/harness/status.ts` is the shared `DerivedState` vocabulary.

## Stable files

These define contracts. Touching them ripples; read them first.

- `src/core/config.ts` — schema, defaults, validation. Loader fails fast at startup with one aggregated error message. Optional sections (`sst`, `linear`, `ai`) are `null` when absent; `requireSst()` is the typed boundary for SST-only code paths.
- `src/tui/rows/types.ts` — `RowModule` contract.
- `src/tui/rows/index.ts` — registry. Adding a row: write the file, append to `REGISTRY`, optionally add to the default `ui.rows` order in `config.ts`.
- `src/tui/hooks/useWorktreeRows.ts` — per-worktree field aggregator. `FieldState<T>` carries `error`; plumb it through `toFieldState` when adding a field.
- `src/core/diff/` — graceful-degradation diff compactor. `parts.ts` parses, `render.ts` transforms per mode, `fit.ts` runs the priority-aware greedy reducer, `index.ts` is the entry. Modes (`full → tight → hunks → dropped`) and priority tiers are the two knobs worth touching here. Cache keys are SHA-256 prefixes of the *unfiltered* diff so filter tweaks don't invalidate prior summaries.
- `src/core/ai.ts` — OpenAI-compatible / Gemini AI client. One call returns `{title, brief, description}` parsed from a `TITLE: …\nBRIEF: …\nDESCRIPTION: …` line-prefixed response. Parser is lenient — falls back to "everything is the description" when the model ignores structure; `brief` falls back to the title then to a hard-truncated description tail.
- `src/core/logger.ts` — structured logger. Two channels per source: file-only `debug/info/warn/error(msg, ctx?)` and `event.{info,ok,warn,err,dim}(text)` which fans out to file *and* the activity pane (when the TUI runtime has registered a sink). Lazy daily file at `<config.paths.appLogDir>/wt-YYYY-MM-DD.log`, 14-day retention, cross-process append-safe. `tui/activity-log.ts` is just the in-memory store + `useEvents` hook — emit through `createLogger(...)` instead.

## Working principles

- **Source fetches stay batched.** The github source is one GraphQL round-trip aliasing every per-worktree PR field (state, checks, reviews, requested + suggested reviewers) plus the repo `mergeQueue` block. Never split it into per-row fetches; rate limits and latency are real. New PR fields go into `PR_FRAGMENT` in `core/github/fetch.ts` rather than getting their own query. Merge-queue position + auto-merge (`autoMergeRequest`) ride the same fetch and surface as `GithubData.mergeQueue` / `pr.autoMerge`; the `m` keybind toggles auto-merge via `gh pr merge --auto` / `--disable-auto`, which enqueues into GitHub's native merge queue when one is configured.
- **AI summary is hash-keyed.** `aiSummaryQuery`'s queryKey is `["aiSummary", <hash>]`, value is `{title, brief, description}`. Equivalent diffs across rebases / amends / branch renames hit the same cache entry — the whole point of content-addressed keying. The "keep the previous summary visible while a new hash fetches" behavior comes from `placeholderData: keepPreviousData` at the observer; when the hash flips, the new key has no entry, the placeholder serves the prior data, and the new fetch overwrites once it lands. No mismatch effect, no memo family, no force-regen flag — the diff hash flipping IS the trigger. `refreshAiSummary` (manual regen via `t`) refetches the diff context and `removeQueries` the AI summary entry for the resulting hash; the active observer re-fires and `keepPreviousData` covers the gap.
- **Pluginify reactively.** Built-ins now, plugins only when there's a second concrete implementation that justifies the seam. Don't pre-design for "what if someone wants Jira."
- **No client-app defaults in code.** `paths.main_clone`, `paths.worktree_root`, `branch.prefix` are required and the loader refuses to start without them. New required fields go through `Errors.reqStr`; add a derivation when one's natural (see `stage.prefix` defaulting from `branch.prefix`).
- **Errors render verbatim, gated on retries-exhausted.** `firstError` in `details.tsx` already handles the gate (`error && !isFetching`) — don't duplicate it elsewhere. The same error showing on every row that depends on a broken source is intentional.
- **Convention over configuration for the niche stuff.** `branch.id_pattern` exists but most users will never set it; the default matches Linear/Jira/Shortcut conventions.
- **Mutating GitHub state? Invalidate `["github"]`, not the worktree.** TUI actions that hit `gh` for a write (mark-ready, edit reviewers, …) must call `refreshGithub()` from `state/hooks.ts` so the badge flips immediately. `invalidateWorktree(slug)` looks plausible but the github query is keyed by branch list, not slug — it'll silently miss and the user will see stale state until the slow staleTime expires.
- **Automations are level + ledger, never edge-triggered.** The `[[automations]]` engine (`core/automations.ts` ledger, `tui/automation-rules.ts` pure evaluation, `tui/hooks/useAutomations.ts` queue + dispatch) re-derives trigger conditions from row state on every pass; once-only comes from persistent fire keys (`~/.cache/wt/automations.json`), keyed on head SHA so a new push re-fires and the same failure doesn't. The intent queue is deliberately NOT persisted — it rebuilds from conditions on boot. Hard rules: `markFiresDispatched` runs synchronously before any await in a dispatch; PR-driven conditions require `githubFresh` (a live fetch this session — persisted-cache data must never fire) plus `pr.headRefOid`; failures mark delivered and never retry (new SHA = the retry); the per-(rule, target) breaker trips after `BREAKER_LIMIT` consecutive no-clear dispatches and resets only when the condition is observed false. New triggers: add to `AutomationTrigger` in config.ts + a case in `automation-rules.ts` with a documented fire key. Runs dispatch through `launchAction` / `doCleanSlugs` / `doRestackStack` — the same paths keystrokes use, never bespoke ones.
- **Freshness is push-based; `r` is a backstop, not the mechanism.** Every external state source has an event trigger that invalidates the matching query: `.git/refs/` watcher (commits/fetches/pushes → github + `["wt"]` + wtState), `.git/worktrees/` watcher (worktree add/remove → worktree list), per-worktree dir watchers (edits → dirty; `.sst/` writes → deploy), `~/.cache/wt/state.json`+`archive.json` watcher (CLI/cross-process stack, section, archive writes), the github-events webhook marker (PR/check churn → github + a staleTime-gated `git fetch origin`), a 3-minute `fetch origin` interval backstop, claude-registry fs.watch, session-tail triggers (`gh pr …`/`git push` inside a Claude session → github), and action `affects` tags on completion. When adding a new state source or mutation path, wire one of these (or an explicit invalidation at the call site) rather than shortening a staleTime or telling the user to press `r` — staleTimes only bound how wrong things can be when a trigger is missed.

## Modal UX rules

Every list-picker modal follows the same shape so muscle memory carries across pickers. Hold to these when adding or modifying a picker; deviations fragment the UX.

- **Trigger-key re-press confirms.** Whatever single key opens the picker (`l`, `;`, `'`, `!`, `v`) also commits the highlighted row when pressed again. Concretely: `l l` = open section picker, confirm current highlight. `; ;` = open sessions, attach. `' '` = open outputs, focus. `! !` = open action picker, run/edit. `v v` = submit reviewers (multi-select special case: "I'm done choosing"). The chord-loop replaces the old "Enter to confirm" requirement so the user never has to hop their hand off the trigger key.
- **Enter still works.** Enter is always a valid confirm. The chord is the cheap path; Enter is the discoverable path.
- **Esc / q / Ctrl+C cancel.** Universal across every modal. No exceptions.
- **j/k or arrows move.** No fancier nav; `g`/`G` aren't bound inside pickers.
- **1–9 quick-pick.** When a list shows ≤9 items, digit jumps + commits in one keystroke. Out-of-range digits are silently ignored.
- **Sub-affordances get their own letter.** Special rows like "+ new section" or "Custom prompt…" used to share the trigger key (`l l` jumped to "+ new section"). Now they get a distinct letter: `l n` = new section, `! c` = custom prompt, `; n` = new claude session. The trigger-re-press always means "confirm the highlight", never "jump to the special row."
- **Live preview on the bottom pane when it helps.** Pickers that map a row to a viewable output (outputs picker, sessions picker) push the highlight into the OutputViewer on j/k via `previewFocusPatch` from `tui/picker-preview.ts`. Pickers without a sensible preview (section, action, reviewer, parent, branch) leave the pane alone.
- **`x` kills.** Where rows represent killable things (claude sessions), `x` on the highlight invokes destroy without an extra confirm.
- **Hints reflect the chord.** Always render the trigger-key-confirm pair in the modal's `hints` prop (e.g. `["l / ⏎", "select"]`). `PickerModal` and `MultiPickerModal` take an optional `toggleKey` prop that wires this for you.
- **No pin.** The bottom pane has no sticky "pin" override anymore. Per-slug focus is a single nullable field; auto-rules surface things in the foreground, explicit picks override until escape or row change.

When a picker doesn't naturally have a single trigger key (e.g. branchPicker is reached mid-`n` flow, not via a dedicated chord), drop the trigger-re-press leg and keep just Enter/Esc. Don't invent a trigger key to satisfy the rule.

## Logging & debugging

- **Daily app log**: `~/.cache/wt/logs/app/wt-YYYY-MM-DD.log`. Every `createLogger(source)` call writes here, regardless of TUI/CLI. Plaintext header (`<iso-ts> <LEVEL> <source>`) with structured ctx as a trailing JSON blob. Read it when something looked wrong: errors thrown deep in queries, stack traces from caught exceptions, or to recover what the activity pane scrolled past (event lines are tagged `EVENT`, so `grep ' EVENT '` shows exactly what the user saw).
- **Per-worktree destroy logs** still live one level up at `~/.cache/wt/logs/<slug>-*.log` (separate from the daily app log) and are surfaced via `wt logs <slug>`.
- **Adding a log call**: `const log = createLogger("[some-source]")` at module top for static sources, or `createLogger(slug)` per call for dynamic ones. Use `log.event.X` when the user should see it in the activity pane; use `log.debug/info/warn/error` for file-only diagnostics. The "errors render verbatim, gated on retries-exhausted" rule still applies — `log.error` does **not** auto-promote to the pane.

## Tooling

- Runtime is **Bun**. No node, no pnpm. `bun install`, `bun src/main.ts`.
- Typecheck: `bun run typecheck` (just `tsc --noEmit`).
- Tests are opt-in, not comprehensive: `bun test` runs the few `*.test.ts` files. The
  one that matters is `src/core/stack-ops/hunks.test.ts` — golden tests pinning the hunk
  engine (`parseFileDiff`/`reconstructFile`) against real `git diff` output, since a
  silent off-by-one there corrupts a slice's committed content. Add cases there when
  touching `core/stack-ops/hunks.ts`. Most other modules still have no tests; smoke via the CLI.
- The TUI takes over the terminal — smoke-test refactors via the CLI subcommands (`bun src/main.ts ls`) or check imports with `bun -e 'import("./src/path.tsx")'`.

## Traps

- **The list panel (`src/tui/panels/list.tsx`) is NOT row-driven.** Different layout (single line of glyphs, no labels), intentionally not wired through `rows/`. Don't try to unify them.
- **Config loads once at module init.** No hot reload — editing the TOML requires restarting `wt`.
- **`Bun.TOML.parse` is built in.** No external TOML lib needed; don't add one.
- **macOS-only utilities** (`open`, `pbcopy`) are assumed. Anything that shells out to them stays guarded by the macOS assumption noted in the README.
