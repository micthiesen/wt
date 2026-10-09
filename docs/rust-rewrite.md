# Rust rewrite execution record

The goal is the complete, one-shot replacement of the production TypeScript wt
with a native Rust application, ready for promotion from `rusty` to `main`.
Promotion itself is not authorized. Baseline commit: `d9cd2f4`.

The user approved implementation, bounded Sonnet/Luna delegation, dependency
installation, local interactive/headless testing, test branches and CI runs.
Libraries and architecture remain revisable when evidence warrants it.

## Acceptance

- Account for all CLI commands, terminal workflows, configuration, integrations,
  internal protocols, and safety contracts in the feature inventory.
- Preserve existing durable data, configuration, and session identity; prove
  compatibility using isolated copies and fixtures.
- Improve measured CPU efficiency and input-to-frame latency versus the current
  implementation under representative idle and loaded workloads.
- Ship without a source checkout or JavaScript runtime. Verify CI, installers,
  existing-install migration, updates, failed updates, rollback, daemon and remote
  worker compatibility through an isolated release channel.
- Complete independent review, appropriate automated and real-environment tests,
  documentation, removal of obsolete production dependencies, commit, and push.
- Record only unavoidable post-promotion checks as such. Do not claim them passed.

## Implementation sequence

1. Extract compatibility inventory and baseline evidence; establish typed,
   explicitly owned configuration, state, process, and scheduling boundaries.
2. Prove an isolated UI loop remains live under slow background work.
3. Implement feature domains in bounded parallel assignments with stable shared
   interfaces, integrating and reviewing each domain.
4. Exercise complete workflows, real terminals, performance, and release recovery.
5. Remove the reference runtime and perform the requirement-by-requirement
   completion audit before handing over the promotion-ready branch.

## Current work

Foundation and native workflow integration remain in progress. `wt-config` owns
configuration discovery/schema; `wt-store` owns durable SQLite state and payload
migrations; `wt-platform` owns subprocess lifetime and resource bounds;
`wt-runtime` owns source scheduling and task lifetimes; `wt-core` owns pure
status, identity, edge and stack rules. `wt-vcs` and `wt-tmux` provide external
adapters. `wt-lifecycle` owns creation/removal safety and filesystem copying.
`wt-harness` has Claude, Codex and OpenCode discovery, lifecycle and message
adapters; application integration and real installed-harness smoke remain owed.
`wt-app` connects services to the event-driven `wt-tui` presentation and exposes
status, base, section, edge, lifecycle, merge, open and destroy-log commands.
The terminal has a shared Unicode line editor and an asynchronous command
channel. Local inventory and batched GitHub reads run in independent source
lanes; a projection prepares their combined board without network work on the
input thread. The updater and stable native launcher have isolated fixture
coverage; config-free app boot probing and confirmation are wired before state
migration. Release CI, installation and real update flows remain integration work.

The current UI slice adds create, remove/cleanup confirmation, archive, status,
fork-base, issue and URL actions, plus explicit terminal handoff for tmux
sessions. A focused review identified confirmation drift and same-slug recreation
risks; checkout revisions are being carried through confirmations and detached
removal jobs and rechecked under the lifecycle lock. A CLI bootstrap command
creates repository config atomically and requires an explicit/inherited branch
namespace so a first installation does not depend on an existing global config.

The native agent/manager/hold commands and embedded skills are being integrated.
Skills crate tests (8) and TUI tests (13) passed in their scoped runs. App-level
testing is deliberately serialized after the current shared-contract edits;
mid-edit compiler output is not recorded as verification.

Native release packaging validates the compiled binary's exact build/target
identity and creates deterministic archives. Four packaging tests pass,
including tampered archives and missing platform refusal. GitHub release
workflow files now exist but have not yet run remotely. The build matrix covers
Apple/Intel macOS and x86/ARM Linux; Linux release builders use Ubuntu 22.04 for
the glibc baseline. Test release tags are separate from automatic preview offers.

No feature parity, performance improvement, or release readiness has yet been
established. The TypeScript tree remains unchanged as the behavior reference.

## Research evidence

- Conversation `01a0fa7d-f6f4-7fc3-9ba5-ef2f3fa70570` L263 measured 480 Git
  launches for a 24-row full refresh and 4.6 seconds of synchronous copying.
  Later fixes in that conversation reduced stalls, so compare against the
  baseline commit rather than claiming those old improvements again.
- `.agents/skills/perf/notes.md` records resolved continuous-rendering and
  query-bookkeeping costs, plus unresolved machine-wide problems that cannot be
  attributed to wt without measurements.
- The existing push-based freshness, scoped refresh, session ownership,
  cancellation, state-namespace and GitHub retry contracts remain requirements.

## Verification log

- Reference `bun test` at `d9cd2f4`: 1,619 passed, zero failed, 4,519 assertions,
  211 files, 37.9 seconds. Log: `/tmp/wt-rust-reference-tests.log`.
- Independent foundation review found five issues. Fixes add regression tests
  for concurrent SQLite initialization/registration, preservation of future
  fields and claim timestamps, residual process-group children after a successful
  leader exit, and factory/future panic recovery in refresh sources. The follow-up
  review closed those findings and identified pure-domain compatibility issues;
  claim metadata and target identity fixes have regression coverage.
- First integrated native checkpoint: `cargo xtask gate` passes formatting,
  strict workspace Clippy, dependency boundaries, and all 231 tests across 21
  binaries (zero skipped), plus doctest targets. Release packaging has four
  additional passing Python tests. This covers the implemented slice; the
  compatibility inventory still has outstanding features.
- The integrated debug binary passes real-PTY slow-Git, filesystem refresh,
  title persistence, accepted-write drain and quit/SIGTERM cleanup checks.
  Key-to-cursor-output latency was 1.06 ms and 1.16 ms during a two-second Git
  delay. Evidence: `/tmp/wt-rust-native-integrated-ui-1/` and
  `/tmp/wt-rust-native-integrated-term-1/`. The detached lifecycle test passes
  acknowledgment, durable removal, process exit, path/head guards, and CLI
  picker SIGTERM cancellation in 1.0 ms. Its stale-job fixture now includes the
  required removal revision so it reaches the intended HEAD guard.
- `cargo xtask hygiene --clean-incremental` reduced local Cargo output from
  15 GiB to 9 GiB by removing old incremental state only. Compiled binaries and
  test evidence were preserved.
- `scripts/native-ui-check.py` passed against the first native release build on
  a real PTY: all three fixture worktrees rendered; no idle frames; a navigation
  key produced the new cursor output in 0.24 ms while Git processes were
  deliberately delayed two seconds; an external file edit invalidated the
  source; normal exit restored the alternate screen and joined owned work.
  Evidence: `/tmp/wt-rust-native-ui-1/result.json` and `terminal.ansi`. This
  validates the initial loop, not full application workload performance.
- Expanded PTY checks passed with title editing persisted to SQLite and both
  ordinary quit and SIGTERM while Git was still running. Navigation took about
  1.1 ms from injection to observed cursor output against the debug build.
  Evidence: `/tmp/wt-rust-native-ui-title-1/` and
  `/tmp/wt-rust-native-ui-term-1/`. These are isolated fixtures, not live user
  state. A regression check additionally covers an accepted title write followed
  immediately by quit while its inventory lookup is delayed.
- Independent CLI/UI review found lost accepted title commands on shutdown,
  wrong edge authorship from a third worktree, and swallowed status-lookup
  cancellation. Fixes separate command drain from source cancellation, resolve
  authors against the complete inventory, and propagate process errors. The
  edge and status regression tests pass. The expanded quit/drain PTY check also
  passes: `/tmp/wt-rust-native-ui-drain-1/result.json` proves the title survived
  immediate quit while its inventory lookup was delayed two seconds.
- `scripts/perf-baseline.py` exercised the existing application in an isolated
  real PTY with 24 linked worktrees for 65 seconds per scenario, including five
  seconds warmup. GitHub, auto-updates, skills checks and automations disabled;
  no agent sessions. Each worktree has its own commit and eight are dirty.

Additional integration evidence:

- First remote CI run at `dcf2b8c`: Linux and macOS tests and release builds
  passed. Linux strict lint found a nested-if warning in its `/proc` process
  identity reader; the follow-up collapses it. CI URL:
  https://github.com/micthiesen/wt/actions/runs/38002199332
- Real shell handoff exposed a blocking cursor-position read in Ratatui's
  `clear()` after tmux detach. Resuming now invalidates fullscreen buffers with
  `resize()` instead. `scripts/native-session-check.py` passes shell input,
  detach, resumed navigation/title save, clean quit and shell survival, using a
  private tmux server. Evidence: `/tmp/wt-rust-native-session-5/result.json`.
  The test drains terminal output through exit and closes its PTY before forced
  reap so fixture teardown cannot hang on macOS.

- Real Rift create/remove and stale registry recovery pass with isolated HOME
  and XDG state. The detached native removal probe checks startup acknowledgment,
  completed durable removal, worker exit, traversal-ID refusal and stale-head
  refusal without deleting the changed checkout.
- Source projection tests prove local updates continue during a remote refresh,
  last-good PR data survives remote failure, failures remain visible during local
  updates, and local title changes do not trigger another GitHub fetch.
- GitHub adapter review fixes preserve string GraphQL variables, fail closed on
  unknown merge-queue inspection, and align asynchronous merge response handling.
  Sixteen scoped tests and Clippy pass. The exact generated GraphQL query was
  accepted by the live API for `micthiesen/wt`, read-only; no PR was mutated.
- The harness domain has 38 passing scoped tests and Clippy. A lost Codex queue
  add reply is reconciled without a second add. This is fixture evidence, not a
  claim that real Codex/OpenCode message delivery has been exercised yet.
- The integrated PTY probe still passes with the composed board, including an
  accepted title write followed immediately by quit. Navigation output appeared
  in 1.13 ms while Git was delayed two seconds; no idle frames. Evidence:
  `/tmp/wt-rust-native-ui-github-1/result.json`. GitHub was disabled for this probe.
- Permanent setup repairs were committed and pushed in dotfiles `4414349`:
  rustup provisioning and repository-specific personal GitHub account routing.
  All 103 dotfiles tests and 29 live component checks passed. These are machine
  setup changes, not wt release-readiness evidence.

| Reference scenario | Waited process-tree CPU | CPU / wall time | Root peak RSS |
|---|---:|---:|---:|
| Idle | 5.607 s | 8.62% | 284.6 MiB |
| Navigation every 80 ms | 18.561 s | 28.51% | 333.4 MiB |
| Refresh every 5 s | 49.013 s | 75.30% | 394.2 MiB |

The CPU figures include startup, source work, and reaped children such as Git;
they are not renderer-only CPU and exclude detached tmux servers. Navigation's
application telemetry reports 646 samples, p50 8 ms, p90 11 ms, maximum 24 ms.
That timer begins at application input dispatch, so an external injected-key to
paint probe is still required. Raw records live under
`/tmp/wt-rust-baseline-24/`. These are baseline measurements, not a claim that
the incomplete native application has achieved performance parity or improvement.

## Build efficiency

The user supplied `/Users/michael/Downloads/omni-notify-main` as the successful
Rust conversion precedent. Adopt its useful mechanisms: line-table-only workspace
debug information with dependency debug symbols disabled; distinct lint/test CI
caches; independent release compilation with publication gated on all checks;
bounded test execution; and measured, scoped Cargo artifact hygiene. Do not copy
its Docker/Wasm pipeline into a native terminal application.

Parallel agents initially share the target directory to avoid recompiling every
dependency per worker. If lock contention warrants separate target directories,
bound their number and include them in hygiene. Cleanup must not race live builds
or remove test evidence. Toolchain/dependency and CI pins will be committed with
the Rust gate before release validation.
