# Native rewrite feature accounting

This is a feature and risk ledger for the Rust replacement. The TypeScript
behavior baseline is commit `d9cd2f4`; its source tree is retired from this
branch. Source paths associated with that baseline are historical references.
Implementation presence, domain tests, CLI comparisons, and real-environment
fixtures are different evidence. This ledger does not treat missing exhaustive
tests as missing implementation, or a passing test as proof of unrelated
behavior.

## Current evidence and cutover state

Application build `4ef9878` passed all 577 workspace tests, doctests,
formatting, strict Clippy, and dependency checks. The final local log is
`/tmp/wt-rust-final-retired-gate.log`. Linux/macOS
[Rust CI 38023495258](https://github.com/micthiesen/wt/actions/runs/38023495258)
passed. The four-target release, published install/update/rollback, and real
macOS-to-Boris provisioning checks passed too, including two-config isolation
and fixture/runtime cleanup. Exact evidence is in [the execution record](rust-rewrite.md).

The final PTY passes cover normal and SIGTERM shutdown, delayed Git navigation,
filesystem refresh, metadata-only writes, accepted-write draining, sections,
history, and feeds: `/tmp/wt-native-ui-retired-final` and
`/tmp/wt-native-signal-retired-final`. The command fixture and nine representative
old/new JSON comparisons passed (`/tmp/wt-cli-compat-retired-final/result.json`).
Independent review findings were repaired and re-reviewed.

The optimized five-scenario comparison is complete: CPU fell 56–88%, peak RSS
fell from 288–386 MiB to 23–25 MiB, and navigation p90 fell from 12 ms to
1.24 ms. See [the measurements and workload limits](rust-rewrite.md#performance-evidence)
and [machine-readable results](rust-rewrite-performance.json). The oracle is
`bbf1696` plus the minimal tmux delimiter repair; original shared goldens remain
tied to `d9cd2f4`.

## Command surface

The native statically linked dispatcher is in `crates/wt-app/src/main.rs` and
`crates/wt-app/src/commands/`; `docs/cli.md` is the user-facing flag and output
contract. Rust adds `archive`, `restore`, and `install`, and aliases `list`,
`remove`, and `cleanup`. The old lazy TypeScript loader is deliberately
replaced by explicit config-free dispatch for `version`, `init`, `install`,
`update`, and `rollback`.

| Commands and retained contracts | Evidence and known differences |
|---|---|
| `init [directory] [--primary]` plus native `--prefix`; `state migrate [--from] [--keep-legacy]`; `version`, `--version`, `-v`. | Config generation/parser tests, migration fixture preserving unknown values and backups idempotently, and config-free version fixture. Native version identifies build/target rather than the old source SHA. |
| `ls [--json]`, `fleet [--json]`; live/removed row `kind`, nullable Git/session/dev facts, PR/stage/issue/section, status and history; `status [slug] [state]` with `-m`, `--risk`, `--blocked-on`, `--verify-after-merge`, `--unblock`, `--clear`, `--all [--json]`, `--note-only`, `--append`, `--examined`. | Native command fixture and nine old/new representative comparisons pass. This is not an exhaustive output snapshot for every failure condition. `by: null` compatibility is fixed. |
| `new <id/title/url/branch/slug>` with `--slug`, `--gh`, `--attach`, `--base`, `--any`, `--open`, `--no-open`, `--no-install`; `rm [slug]` with `--yes|-y`, `--force`, `--destroy-stage|--no-destroy-stage`, `--delete-branch|--keep-branch`, `--background|-b`; `clean` with `--yes|-y`, stage flags, `--foreground|--background`. | Parser, lifecycle, cleanup, and resource fixtures cover argument separation, create/remove safety, exact resource ownership, stale revision refusal, retention, archive/history, and idempotence. CLI output is allowed to differ where `docs/cli.md` states the native contract. |
| `doctor [slug] [--all|-a] [--json]`; `stages [--clean] [--yes|-y] [--json]`; `logs [slug]`; `perf [--json]`; `open [slug-or-query]`; `base [show|set|clear]`; `merge [slug] [--cancel]`. | Doctor/stage/perf command fixtures and focused domain/service tests exist. Stage safety uses fake AWS/pnpm; GitHub mutation tests fail closed. Editor, log and CLI text edge cases have less direct fixture coverage, but no missing handler is known. |
| `edge [from kind to]` with `--blocks`, `--prefer`, `-m`, `--json`, `rm`, `prune`; `section [list|mv|rename|rm]` with `--only`, `--json`, `ls|move|remove`; `restack [branch] [--onto]`, `prune-backups [--days]`. | Domain and real-Git tests cover expiring edges, SHA anchors, sections, stack replay, conflicts, leases, backup pruning and cancellation. These additions and domain-focused evidence replace a requirement for exhaustive flag-by-flag output goldens. |
| `manager`, `manager send [--hold <id>] <text...>`, `manager report [--info|--ok|--warn|--err] <text...>`; `hold [set|release|check]`; `issue [--id|--no-id|--clear-id|--gh|--clear-gh|--read]`. | Hold/report/state tests and issue CLI fixture cover durable semantics, output routing, reader stderr/exit propagation, and identity. |
| `skills [status|sync|diff|reset]`; `sync [names...] [--yes|-y] [--force]`, `reset [--answers|--declines]`, legacy `install`; `update [log] [--check] [--head]`; `rollback [ref]`. | Current parser accepts multiple sync units and `-y`; skill sync protects modified copies. Native release fixtures cover update/install/rollback and recovery. `--head` is intentionally rejected because source-clone updates were retired. |
| `events install|start|stop|restart|status|secret|uninstall|serve`; `remote [argv…]`; `agent send <target> [text...] [--hold <id>]`, `start <slug>`, `ls [--json]`; `claude send` alias, `ls [--json]`, `selftest [slug]`, `stop|kill <slug>`; `codex selftest`; OpenCode through `agent`. | Isolated loopback SSH, host, session, action, event and cleanup fixtures cover their named protocol/lifecycle behavior. Boris adds real native remote provisioning. Claude Code's external Bun inspector remains an integration requirement. Native fixtures do not assert successful live delivery through every user's installed agent CLI. |
| `dev start|reset [--wait] [--timeout] [--rebuild]`, `stop`, `status [--all] [--json]`, `logs`, `queue [slug] [--first|--normal] [--json]`, `--lines`; help and unknown-command exit 2. | `native-dev-check.py` covers real local process health, queue, cancellation, logs and cleanup. Hosted macOS dev fixture passed at the 4ef9878 workflow. Static linking intentionally replaces TypeScript import-failure isolation. |
| Internal `_remote`, `_hello`, `_snapshot`, `_session`, `_host`, `_destroy`, `_dev-giveup`, `_claude-hook`, `_action-worker`, `_restack-worker`, `_dev-supervise`. | Protocol/lifecycle tests cover framing, exact argv/build, errors, cancellation, and worker recovery. `_claude-hook` is a native no-op compatibility endpoint. |

Representative old/new CLI comparisons are useful evidence, not a mandate to
match every incidental string, every flag's exact error text, or every JSON
field byte-for-byte. Keep user-facing changes explicit in `docs/cli.md`; retain
the actionable safety, state, exit-code, and machine-readable contracts.

## TUI workflows and known differences

`docs/tui.md` and `crates/wt-tui` define the native UI. The feature surface
covers list/details/activity; section and stack layout; remote rows; identity-
anchored selection; folded summaries; navigation, sort, history and refresh;
create/remove/archive/restore/clean; editor, issue/title, tracker and yank;
PR actions; tmux/harness sessions; section/base/status/restack; automation
controls; performance/error views; clipboard and help; picker/modal
semantics; Unicode width, clipping, wrapping, scroll/follow, resize, mouse and
links. The current source refinements are covered by independent review and
the focused regressions; the integrated verification is tracked above.

PTY fixtures prove their named scenarios: delayed Git while navigating,
filesystem refresh, title writes and quit drain, sections/folding/history,
tmux handoff/detach/resume, and cleanup. Do not infer all TUI behavior from one
probe. Known deliberate or informational differences:

- Remote worktree creation does not queue F12 attach; after the row appears,
  select it and press F12.
- Special-session footer indicators are status text; actions remain available
  through keybindings and command palettes.
- Context-window occupancy is shown for a live manager Claude session when
  transcript data is available. Codex/OpenCode do not supply equivalent
  per-turn context facts.
- Completed-session transcript summaries are shown for selected Claude output,
  not as a generic banner for all harnesses.
- Rows do not display an explicit title-source badge.
- A fatal TUI failure restores the terminal and reports the error. Recovery is
  a fresh launch rather than the former in-app crash/retry overlay.

These are documented behavior differences, not automatically blockers to a
non-exact conversion. Decide product importance from observed use rather than
inflating the test checklist.

## Configuration, integrations, state, and safety

`wt-config` preserves TOML merge/default/validation, aliases, required paths,
and repository identity independent of caller cwd. Golden fixtures compare all
supported blocks with the baseline schema. Blocks include `[instance]`,
`[paths]`, `[tmux]`, `[branch]`, singular/plural `[remote]`, `[stage]`,
`[lifecycle]`, `[backend]`, `[deploy.sst]`, `[dev_server]`, `[issue_tracker]`,
`[harness]`, `[naming]`, legacy `[browser]`, `[github]`, `[review_bot]`,
`[github.events]`, `[diff]`, `[editor]`, `[ui]`, `[skills]`, `[manager]`,
`[update]`, `[[actions]]`, and `[[automations]]`. Schema evidence does not
substitute for runtime integration fixtures.

| Domain | Native ownership and proof scope | Intentional change or remaining evidence limit |
|---|---|---|
| Git and Rift backends, origin/ref freshness, inventory and stack replay | `wt-vcs`, `wt-lifecycle`, `wt-stack`; isolated real-Git create/remove, registry, replay, lease and cleanup fixtures. | Rift copies project files and does not synchronize package installs. Rift lookup checks process PATH then bounded login-shell PATH. |
| SST, issue tracker, GitHub/merge queue, editor/diff, events daemon, dev server | `wt-sst`, `wt-app`, `wt-github`, `wt-events`, `wt-dev`; fake cloud tools, parser/error tests, batched/fail-closed API tests, signed loopback webhook, real temporary dev process. | No live AWS deletion was used. Hosted macOS service checks and real external account mutations are distinct from fixtures. |
| Claude, Codex, OpenCode and common session lifecycle | `wt-harness`; identity/discovery/usage/output, Claude inspector, Codex app-server receipts, OpenCode state, live-target routing and resource fixtures. Session env pins selected `WT_CONFIG` and launcher PATH. | Real Boris proves remote binary/session environment, not real message delivery for every agent. Claude inspector is provided by external Claude Code/Bun. |
| SQLite, migrations, archives, status/title, sections, fork-base/base SHA, action history, automation ledger, harness registries and holds | `wt-store`, `wt-core`, `wt-actions`, `wt-automations`; lossless unknown-field/future-version tests and isolated migration/recovery fixtures. | SQLite is durable authority; derived source snapshots are rebuildable and cannot erase accepted writes or history. |
| Freshness, batching, last-good snapshots and whole-fetch failure | `wt-runtime` and source crates; tests preserve good state while exposing failed reads, batch API calls, and fail closed on incomplete GitHub chunks. | Replaces persisted TanStack query cache with source-owned prepared snapshots. |
| Skills, managed instructions and remote provisioning | `wt-skills`, `wt-app`; tests protect modified files, managed-block boundaries, symlink/rulesync targets, templates and stamps; Boris remote setup. | `wt skills sync` parser compatibility is implemented; additional template-answer CLI tests are optional evidence, not a known code gap. |
| Automations and manager | `wt-automations`, `wt-actions`, manager/app code; once-only claims, settle/dedupe, breaker, cancellation, durable ambiguity, hold, spool and action fixtures. | Durable guarantees are tested at domain/service boundaries; not every manager CLI rendering path has a bespoke test. |
| Update, installer and remote runtime | `wt-update`, `wt-app`, `wt-launcher`, `wt-remote`; immutable checksum/build/target, health probe, rollback, recovery and exact remote build tests; real isolated install and Boris run. | Replaces source-clone/Bun update and remote source upload. `--path` is required before PATH changes or legacy migration. |

Cross-cutting safety contracts remain load-bearing:

- Load one merged config per process. Pin selected config and active launcher
  PATH into each managed session, not tmux server-global state.
- Preserve durable data and unknown fields. Keep `baseSha` as the squash-safe
  anchor; stack membership comes only from fork-base records.
- Unknown GitHub, inventory, host, or stage state is not proof of safety.
  Destructive cleanup is scoped, idempotent, confirmed, and revalidated under
  its lock. Dirty/unpushed work and owed verification remain hazards.
- Do not retry ambiguous external mutations. Codex lost replies reconcile
  without a second add; OpenCode without a receipt does not claim completion.
- Own and cancel each worker/process/daemon. Drain accepted writes before
  shutdown. Keep I/O and subprocesses off the TUI input path.
- Install immutable checksummed builds; probe before activation and provision
  the exact remote build. Logs are bounded and safe for terminal output.

## Acceptance

The application and pre-promotion evidence gates are complete. Post-promotion
release checks and recovery operations are recorded in
[the execution record](rust-rewrite.md#promotion-and-recovery); they are not
claims that main or stable publication has happened.
