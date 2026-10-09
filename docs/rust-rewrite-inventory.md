# Rust rewrite compatibility inventory

This is the acceptance ledger for a full Rust rewrite. Each `pending` item
needs an implementation and evidence before the matching TypeScript behavior
can be retired. A source file or passing test is not evidence that the Rust
version is covered. The TypeScript tree remains the behavior reference; docs
are the user contract. This inventory deliberately names behavior and test
surfaces so a large rewrite cannot silently drop an obscure command or flow.

Status: `pending` = no Rust parity evidence recorded; `verified` = Rust behavior
and its required compatibility evidence are reviewed. No item is verified yet.

## Command surface

The dispatcher is `src/cli/index.ts`. Preserve lazy per-command loading and
the recovery commands' independence. `docs/cli.md` is the flag and output
contract; command files below own argument parsing and behavior. A test path of
`—` means no focused command test was found during inventory, not that behavior
is covered elsewhere.

| Check | Command / contract | TypeScript reference | Existing evidence | Rust |
|---|---|---|---|---|
| [ ] | `init [directory] [--primary]`; repo config bootstrap | `src/cli/commands/init.ts` | `init.test.ts`; cli `wt init` | pending |
| [ ] | `state migrate [--from] [--keep-legacy]`; idempotent legacy import | `state.ts` | `state.test.ts`; cli `wt state` | pending |
| [ ] | `ls [--json]`; live plus removed rows, nullable facts and `kind` | `ls.ts` | `ls.test.ts`; cli `wt ls` | pending |
| [ ] | `fleet [--json]`; asserted status vs session/PR reality | `fleet.ts` | `fleet.test.ts`; cli `wt fleet` | pending |
| [ ] | `new <id/title/url/branch/slug>`; `--slug`, `--gh`, `--attach`, `--base`, `--any`, `--open`, `--no-open`, `--no-install` | `new.ts`, `agent-args.ts` | `agent-args.test.ts`; cli `wt new` | pending |
| [ ] | `rm [slug]`; `--yes|-y`, `--force`, `--destroy-stage|--no-destroy-stage`, `--delete-branch|--keep-branch`, `--background|-b`; safety guards and cleanup | `rm.ts` | —; cli `wt rm` | pending |
| [ ] | `clean`; `--yes|-y`, `--destroy-stage|--no-destroy-stage`, `--foreground`; merged/gone sweep, owed verification and hazard retention | `clean.ts` | `clean.test.ts`; cli `wt clean` | pending |
| [ ] | `doctor [slug] [--all|-a] [--json]`; config, backend, skills, harness and resource diagnostics | `doctor.ts` | `doctor.test.ts`; cli `wt doctor` | pending |
| [ ] | `stages [--clean] [--yes|-y] [--json]`; list, cleanup and orphan handling | `stages.ts` | —; cli `wt stages` | pending |
| [ ] | `logs [slug]`; tail/fallback to saved destroy output | `logs.ts` | `logs.test.ts`; cli `wt logs` | pending |
| [ ] | `perf [--json]`; bounded process snapshot and downstream classification | `perf.ts` | —; cli `wt perf` | pending |
| [ ] | `open [slug-or-query]`; editor target resolution | `open.ts` | —; cli `wt open` | pending |
| [ ] | `base [show|set|clear]`; recorded fork base and squash-safe anchor | `base.ts` | `base.test.ts`; cli `wt base` | pending |
| [ ] | `merge [slug] [--cancel]`; queue-aware arm and actual-state cancel | `merge.ts` | —; cli `wt merge` | pending |
| [ ] | `status [slug] [state]`; `-m`, `--risk`, `--blocked-on`, `--verify-after-merge`, `--unblock`, `--clear`, `--all [--json]`, `--note-only` | `status.ts`, `core/work-status.ts` | `status.test.ts`, `core/work-status.test.ts`; cli `wt status` | pending |
| [ ] | `edge [from kind to]`; expiring before/conflict/enables assertions | `edge.ts` | `edge.test.ts`; cli `wt edge` | pending |
| [ ] | `section [mv|rename|rm]`; manual batching and remote-qualified state | `section.ts` | `section.test.ts`; cli `wt section` | pending |
| [ ] | `manager`, `manager send [--hold <id>] <text...>`, `manager report [--info|--ok|--warn|--err] <text...>`; singleton attach, durable message route, live structured report spool | `manager.ts` | —; cli `wt manager`, `docs/manager.md` | pending |
| [ ] | `hold [set|release|check]`; bounded resource event and release watermark | `hold.ts` | `hold.test.ts`; cli `wt hold` | pending |
| [ ] | `issue [--id|--no-id|--clear-id|--gh|--clear-gh|--read]`; tracker/GitHub identities | `issue.ts` | `issue.test.ts`, `issue-read.test.ts`; cli `wt issue` | pending |
| [ ] | `restack [branch] [--onto]`; `prune-backups [--days]`; lock, conflict handoff, squash replay | `restack.ts` | stack/stack-ops tests; cli `wt restack` | pending |
| [ ] | `skills [status|sync|diff|reset]`; `sync [names...] [--yes|-y] [--force]`, `reset [--answers|--declines]`, legacy `install`; explicit units and template answers | `skills.ts`, `core/skills/` | `core/skills/*test.ts`; cli `wt skills` | pending |
| [ ] | `update [log] [--check] [--head]`; fetch, gate, journal and startup contract | `update.ts` | update tests; cli `wt update`, `docs/updates.md` | pending |
| [ ] | `rollback [ref]`; safe rollback and declined SHA | `rollback.ts` | update tests; cli `wt rollback` | pending |
| [ ] | `version`, `--version`, `-v`; current source SHA | `version.ts`, `cli/index.ts` | —; cli `wt version` | pending |
| [ ] | `events install|start|stop|restart|status|secret|uninstall|serve`; launchd ownership, config reconciliation and foreground daemon | `events.ts` | `events.test.ts`; cli `wt events`, `docs/github-events.md` | pending |
| [ ] | `remote [argv…]`; interactive SSH and safe exact-argv forwarding | `remote.ts` | `remote.test.ts`; cli `wt remote` | pending |
| [ ] | `agent send <target> [text...] [--hold <id>]`, `start <slug>`, `ls [--json]`; stdin, live-target routing, native queue receipts, skill provisioning | `agent.ts`, `agent-args.ts` | `agent-args.test.ts`, `worker-role.test.ts`, harness routing/messaging tests; cli `wt agent` | pending |
| [ ] | `claude send` compatibility alias, `ls [--json]`, `selftest [slug]`, `stop|kill <slug>` | `claude.ts` | `claude.test.ts`; cli `wt claude` | pending |
| [ ] | `codex selftest`; native queue and app-server control transport diagnostics | `codex.ts` | Codex messaging/readiness/startup tests; cli `wt codex` | pending |
| [ ] | `dev start|reset [--wait] [--timeout] [--rebuild]`, `stop`, `status [--all] [--json]`, `logs`, `queue [slug] [--first|--normal] [--json]`; startup health, limits, wait and priority | `dev.ts`, `core/dev-server.ts` | `dev-server*.test.ts`; cli `wt dev` | pending |
| [ ] | `--help`, `-h`, per-command help; unknown command exit 2; load vs run error distinction | `cli/index.ts` | No direct dispatcher unit test found; `scripts/broken-module-check.sh` probes failure containment | pending |
| [ ] | Internal `_remote`, `_hello`, `_snapshot`, `_session` worker protocol; versioning, framing, error/exit behavior | `_remote.ts`, `_hello.ts`, `_snapshot.ts`, `_session.ts` | remote tests; protocol docs under `docs/cli.md` | pending |
| [ ] | Internal `_destroy`, `_dev-giveup`, `_claude-hook`; lock handoff, bounded cleanup, hook contract | `_destroy.ts`, `_dev-giveup.ts`, `_claude-hook.ts` | `_dev-giveup.test.ts`; integration tests | pending |

For each row, enumerate the exact accepted flags and aliases from `docs/cli.md`
and command usage in the Rust parser. Preserve stdout vs stderr, exit codes,
TTY-only prompts, JSON field names/nullability, and `WT_CONFIG`,
`XDG_CONFIG_HOME`, `WT_NO_HINTS`, and command-specific environment behavior.

## TUI workflows and interaction contract

| Check | Workflow that must survive | Source/evidence | Rust |
|---|---|---|---|
| [ ] | List/details/activity layout, sections, stack rails, remote rows, cursor anchored to identity, folded-section summary | `docs/tui.md#layout`; `docs/stacked-prs.md`; `src/tui/` | pending |
| [ ] | Navigation, sorting, history `h`, cursor retention/removal fallback, refresh `r`/`Ctrl+R` | `docs/tui.md#keymap`; `src/tui/normal-keys.ts`, `useVisualItems.ts` | pending |
| [ ] | Create, remove, archive/restore, clean, editor, issue/title, tracker and yank actions | `docs/tui.md#worktree-actions`; `src/tui/flows/` | pending |
| [ ] | PR open/ready/ship/reviewer/check logs/queue-aware merge | `docs/tui.md#pull-request`; `src/tui/flows/` | pending |
| [ ] | tmux shell/diff/agent sessions, named sessions, harness pick/cycle, manager and persistent slots | `docs/tui.md#sessions`; `src/core/tmux/`, `src/core/harness/` | pending |
| [ ] | Section move/base/work status; restack success, conflicts and chained lock behavior | `docs/tui.md#organize`; `docs/stacked-prs.md` | pending |
| [ ] | Automation pause/resume/cancel and status/history/key triggers | `docs/tui.md#automations`; `docs/automations.md` | pending |
| [ ] | Perf overlay, event-driven idle rendering, key-to-painted-frame latency, resize/narrow terminal behavior | `docs/tui.md#perf-overlay-p`; `docs/architecture.md#rendering--input-latency`; perf notes | pending |
| [ ] | Error overlay, crash view, clipboard, captured errors and restart warning | `docs/tui.md#error-overlay`; `src/tui/` | pending |
| [ ] | Pickers and modal conventions: selection visible after resize/reorder; Esc/confirm semantics; footer-input dispatch; modal-first routing | `docs/tui.md#picker-conventions`; `docs/architecture.md#modal-ux-rules`; `src/tui/modal-keys/` | pending |
| [ ] | Keyboard/help legend, Unicode display-cell width, clipping, wrapping, scroll/follow behavior, tmux mouse/link and clipboard contract | `docs/tui.md#keymap`; `src/tui/panels/help.tsx`; TUI render tests | pending |

Navigation and keymap groupings are documented under `Navigation`, `Worktree
actions`, `Pull request`, `Sessions`, `Organize`, `Automations`, `P`, error
overlay and `h`; port key behavior from those entries and the handlers, not
from the table alone. The current interaction order is modal, footer input,
removed view, `h`, normal keys. Preserve creation's pending-key-until-visible
selection rule.

## Configuration, integrations, and data

| Check | Contract | Source/evidence | Rust |
|---|---|---|---|
| [ ] | Parse TOML, required paths, fail-fast validation, aliases/deprecations, defaults and repository identity independent of cwd | `src/core/config.ts`; `docs/configuration.md` | pending |
| [ ] | Config blocks: `[instance]`, `[paths]`, `[tmux]`, `[branch]`, `[remote]`, `[stage]`, `[lifecycle]`, `[backend]`, `[deploy.sst]`, `[dev_server]` | `docs/configuration.md`; `src/core/config.ts` | pending |
| [ ] | Config blocks: `[issue_tracker]`, `[harness]`, `[naming]`, legacy `[browser]`, `[github]`, `[review_bot]`, `[github.events]`, `[diff]`, `[editor]`, `[ui]` | same | pending |
| [ ] | Config blocks: `[skills]`, `[manager]`, `[update]`, repeated `[[actions]]`, `[[automations]]`; templates, validation, action/automation requirements | same; `src/core/config.ts` | pending |
| [ ] | `git-worktree` and `rift` backends; base/ref freshness, self-healing registry, remote orthogonality and known limits | `docs/backends.md`; `src/core/backend/` | pending |
| [ ] | SST stage pin/cleanup; configured issue tracker commands; GitHub REST/GraphQL and queue; webhook daemon; editor/diff command; remote SSH worker; dev server supervisor | configuration and feature docs; `src/core/integrations/`, `src/core/github/`, `src/core/dev-server.ts` | pending |
| [ ] | Harnesses Claude, Codex and OpenCode; primary selection vs live-target routing; session names, usage, summaries, events, output tails and message transports | `src/core/harness/types.ts`, `src/core/harness/`; harness tests | pending |
| [ ] | Harness interface: list/discover, spawn/resume args, tmux name identity, single-slot semantics, derived state/extras, trust, injection-landed check, reap | `src/core/harness/types.ts`; `registry.test.ts`, `primary.test.ts`, `session-selection.test.ts`, `live-target.test.ts`, `agent-routing.test.ts`, `completion.test.ts` | pending |
| [ ] | Claude: JSONL sessions, named-session identity, trust, questions, summaries, usage, tail worker/parse, inspector injection and fallback | `claude/harness.test.ts`, `jsonl.test.ts`, `names`/`sessions`/`question`/`summary`/`usage`/`tail`/`trust`/`inject/*` tests | pending |
| [ ] | Codex: app-server queue ambiguity/reconciliation, slot UUID stamps, discovery/output workers, rollout cache, readiness, native status and usage | `codex.test.ts`, `app-server.test.ts`, `messaging.test.ts`, `slot.test.ts`, `discovery.test.ts`, `events.test.ts`, `live-identity.test.ts`, `readiness.test.ts`, `rollout-cache.test.ts`, `startup.test.ts`, `native-status.test.ts`, `usage.test.ts` | pending |
| [ ] | OpenCode: session identity, discovery, events and usage; shared session choice, completion, messaging, state and tail workers | `opencode.test.ts`, `opencode/events.ts`; `session-messaging.test.ts`, `tail-worker.test.ts`, `compact.test.ts`, `status.ts` | pending |
| [ ] | Durable state: SQLite schema/migrations/repository ID, wtstate payload migrations, archives/removal history, sections, fork base + base SHA, work status/title, automation once-only ledger, harness registries, communication holds | `docs/updates.md#evolve-data-compatibility-across-hot-updates`; `core/state-db.ts`, `core/wtstate/` | pending |
| [ ] | Query cache shape/versioning, persisted query invalidation, event-driven freshness, last-good data and whole-fetch failure semantics | `docs/architecture.md#freshness-model`; `src/state/` | pending |
| [ ] | Self-update startup check, CI gate, boot sentinel, rollback, clean/ahead refusal and data compatibility policies | `docs/updates.md`; `src/core/update/`, update/rollback/version commands | pending |
| [ ] | Skills/instructions discovery, stamps, modified-copy protection, rulesync, templates, target topology and remote provisioning | `docs/skills.md`; `src/core/skills/` | pending |
| [ ] | Automations: level-triggered dedupe, triggers, dispatch, breaker, pause/cancel, external side effects and action ledger | `docs/automations.md`; `src/core/automations/` | pending |
| [ ] | Manager singleton lifecycle, command palette, report channel, agent targeting/delivery, durable ambiguity, transient holds | `docs/manager.md`; `src/core/manager/`, harness messaging | pending |
| [ ] | Stacks inferred only from fork-base records; trunk normalization, `baseSha` anchor, conflict-safe restack and ephemeral merge edges | `docs/stacked-prs.md`; `src/core/stack-layout.ts`, `core/stack-ops/`, `core/merge-edges.ts` | pending |

## Instructions, docs, scripts, and CI migration audit

This is a read-only review of current guidance. Do not carry language/runtime
instructions into the Rust tree by renaming paths mechanically. Keep user
contracts and measured behavioral evidence, then replace only their obsolete
implementation paths and tools. `CLAUDE.md` contains `@AGENTS.md`; preserve that
compatibility include rather than creating a second instruction source.

| Check | Current artifact and stale or still-valid guidance | Rust rewrite implication | Status |
|---|---|---|---|
| [ ] | `AGENTS.md:10,21-22,34-41`: Bun/React/OpenTUI/TanStack stack; `src/core/config.ts`, `.tsx` help/details paths; Effect, React lifecycle and test-clock conventions; TypeScript lazy imports and checker | Replace stack, source paths, and Effect/React/TanStack/test commands with the chosen Rust architecture. Preserve the three-layer ownership, typed errors, cancellation, modal-first key dispatch, lazy command blast-radius rule, and per-command failure isolation in Rust-specific terms. Keep creation selection delayed until the actual row is in rendered inventory, but remove React/`visualItems` names. | pending |
| [ ] | `AGENTS.md:127-131,137-145,154`: Bun Inspector inheritance/shims, `Bun.TOML.parse`, bare-promise spin, `useTimeline`, `Bun.spawn`, Bun test harness | Replace runtime-specific diagnostics where wt's own process changes. **Do not delete** the Claude/Codex Inspector protocol or PATH shim requirements solely because wt is Rust: they still govern external Bun-based harnesses and child tools. Preserve the general rules against busy waits, render-thread blocking, unsafe inherited environment, and non-hermetic subprocess tests. | pending |
| [ ] | `CLAUDE.md` is the one-line `@AGENTS.md` compatibility include; `AGENTS.md` is canonical | Keep both harnesses loading one canonical project instruction file. No copied `CLAUDE.md` contents. | pending |
| [ ] | `.agents/skills/perf/SKILL.md:23-29,45-47,59-71` uses `WT_PERF=1`, `event-loop blocked`, app-log grep and Bun/render-thread signatures; `notes.md` holds the measured TUI and creation history | Replace active commands/signatures with Rust runtime CPU and end-to-end key-to-painted-frame probes. Preserve dated measurements and resolved TS-era incidents as historical evidence; keep open issues explicitly open (Brave freeze, Codex updater/drain ownership, native cron startup, duplicate destroy fetch). | pending |
| [ ] | `skills/instructions.md` is the always-loaded lifecycle/testing/ownership contract; bundled `skills/{wt,start,restack,manager,handoff,shepherd,babysit,triage}/SKILL.md` teach wt commands and workflow | Keep command/status semantics and human/agent ownership guidance. Update command examples only when the public CLI contract changes. They do not require TS implementation paths; do not rewrite them as a side effect of the language migration. | pending |
| [ ] | `docs/architecture.md:3-4,27-41,94-164,173-330,414-629` maps TS modules, React panes, TanStack cache, Effect boundaries, workers, OpenTUI and stable `.ts/.tsx` files | Rewrite as the Rust internals map after the crate/module layout is established. Preserve dataflow, state freshness, failure, renderer, modal, logging, update and stability contracts as concepts. | pending |
| [ ] | `docs/configuration.md`, `docs/tui.md`, `docs/cli.md`, `docs/automations.md`, `docs/github-events.md`, `docs/stacked-prs.md`, `docs/backends.md`, `docs/manager.md`, `docs/updates.md`, `docs/skills.md`, `docs/fleet.md` | Treat as user-facing behavioral contracts and update links/source details alongside changed behavior. Replace implementation references such as `bun install`, TypeScript paths and Bun boot probes; preserve external integrations and semantics. `docs/manager.md`'s React fiber reference is to Claude Code's external UI and remains relevant to injection, not wt's renderer. | pending |
| [ ] | `docs/cli.md:132,140,375`; `docs/github-events.md:106`; `docs/manager.md:63,178,185` include Node package checks/update install, Bun fallback path, Bun-run harness smoke, Inspector shims and Effect sleep | Decide which contracts survive. The Bun fallback and install references for wt itself should reflect the Rust artifact. The Bun Inspector/shim behavior and external manager smoke still apply where they target Bun-based harnesses. Replace Effect sleep with a cancellation-aware Rust wait without changing serialized delivery behavior. | pending |
| [ ] | `README.md:17,39-51` requires Bun and installs via `bun install`; says Rift carries `node_modules` | Replace runtime/toolchain and install instructions with Rust binaries/build or packaging. Retain `git`, tmux, font, OS and optional integration requirements that still apply. Explain what the backend copies after Rust removes its own Node dependency. | pending |
| [ ] | `scripts/broken-module-check.sh`, `fixture.sh`, `tui-test.sh`, `remote-runtime-install.sh`, `codex-compact-recognition.ts`, `codex-palette-smoke.ts` invoke `bun src/main.ts`, Bun APIs or import `src/**/*.ts` | Replace runtime invocation and source imports with the Rust binary/test seams. Keep fixture isolation, command-module failure containment, real tmux/session smoke, remote content-hash provisioning and exact handshake checks. `BUN_INSPECT` handling in harness-launch tests remains relevant. | pending |
| [ ] | `.github/workflows/ci.yml` installs Bun and runs lint/typecheck/build/test; `discord-digest.yml` runs a Bun TypeScript utility; `discord-ci-alert.yml` reports CI failures | Replace core CI with Rust toolchain setup/cache, formatting/lint, clippy, unit/compat tests, build and supported-target checks. Port or package the standalone Discord helper separately so community automation does not disappear with Bun. | pending |
| [ ] | `docs/skills.md` cites `src/core/skills/` and `registry.test.ts` budget; `AGENTS.md` owns the historical incident ledger | Move source/test pointers to Rust locations while preserving the skill registry line budget, replacement-only rule for `skills/instructions.md`, brand-neutral bundled content, and the distinction between incident evidence (`AGENTS.md`) and terse always-on rules. | pending |

## Cross-cutting invariants and acceptance gates

| Check | Required invariant or gate | Evidence/source | Rust |
|---|---|---|---|
| [ ] | I/O, subprocesses, waits, retries, resource acquisition and concurrency have explicit cancellation and ownership; no blocking filesystem/process work on the input thread | `docs/architecture.md#effect-boundary`, `#controller-and-worker-execution` | pending |
| [ ] | TUI rows stay presentation-only; sources batch by domain, never fetch once per row; stale-time changes do not replace missing invalidation | `docs/architecture.md#the-three-layers`, `#freshness-model` | pending |
| [ ] | CLI command imports remain isolated so a broken leaf does not disable status, update, rollback or other recovery commands | `cli/index.ts`; `scripts/broken-module-check.sh` | pending |
| [ ] | Long-running worker/daemon ownership, shutdown/drain, restart detection and duplicate prevention are explicit; unknown liveness never means stopped | `docs/architecture.md#controller-and-worker-execution`; `docs/manager.md`; `docs/updates.md` | pending |
| [ ] | Logs are structured, actionable, bounded and safe for terminal output; no secrets or untrusted control sequences | `docs/architecture.md#logging`; `src/core/logger.ts` | pending |
| [ ] | Destructive cleanup is guarded, idempotent, scoped, and keeps owed verification/history; no cleanup inferred from a stale cache | CLI lifecycle docs; worktree/backend tests | pending |
| [ ] | Pure rules use language-neutral golden fixtures; Rust and reference runner consume same JSON and compare normalized output | `test/compat/README.md`, `test/compat/*.json` | pending |
| [ ] | Existing unit and integration suites have named Rust counterparts; test-only behavior is not treated as product behavior | `src/**/*.test.ts`; repository test scripts | pending |
| [x] | Local verification and dependency-direction gate; package-scoped runs must not claim workspace green; target cleanup must preserve evidence | `crates/xtask/`, `.cargo/config.toml`, `.config/nextest.toml`, `docs/development.md` | tooling implemented; xtask tests, Clippy and dependency check pass; workspace gate pending |
| [ ] | `broken-module-check.sh`, lint/typecheck, build, full tests, distribution/install/update smoke and startup/rollback probes pass before cutover | `package.json`, `.github/workflows/ci.yml`, `docs/updates.md` | pending |
| [ ] | Real terminal probe records idle CPU and end-to-end key-to-painted-frame latency during creation, sweep, refresh and session churn | prior measured acceptance in `.agents/skills/perf/notes.md` | pending |
| [ ] | Before removing TypeScript, command-by-command old/new stdout, stderr, exit codes, JSON schemas, state migration and representative TUI interactions are compared | this inventory + compat runner | pending |

Performance acceptance should include the known regressions and outcomes:
idle renderer CPU near the corrected ~0–1% baseline (alert if above 5%); no
continuous live-render duty while idle; measure key-to-painted-frame latency
(not just event-loop gaps); and exercise 24-row refresh, large copy, subprocess
bursts, cleanup sweeps and agent output tailing. The historical creation test
reduced the worst timer gap from 4.62 s to 54 ms while total fixture creation
changed 8.96 s to 9.40 s: responsiveness and throughput are separate measures.
See `.agents/skills/perf/notes.md` for measurement limits and unresolved issues.
