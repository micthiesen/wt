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
input thread. The updater and stable native launcher pass isolated installation,
update and rollback flows with the actual binaries. Config-free app boot probing
and confirmation run before state migration. Hosted release publication and
download remain owed.

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
CI is green on Linux and macOS at `cb34d62`; the four-target hosted release
workflow has not yet run. The release build matrix covers
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

- The expanded workspace gate passes formatting, strict workspace Clippy,
  dependency boundaries, all 365 tests across 29 binaries, and doctest targets
  (nextest run `0cd5d1a9-671c-4208-b935-e7df12cbe91e`). Packaging's four Python
  tests and both native workflows' actionlint checks pass. Subsequent review
  fixes still receive scoped checks and real fixtures before the next checkpoint.
- Origin maintenance now keeps generated-file restoration, ref fast-forwarding,
  and lockfile-gated dependency installation under one repository lock. Tests
  cover newly introduced lockfiles, no reinstall after code-only pulls, staged
  generated-file preservation, invalid root paths, and install-failure warnings.
  Shared package-manager detection serves both new worktrees and main-clone sync.
- Independent events review found stale remote-coverage authority, unbounded
  pre-header HTTP connections, and ignored service unload failures. Coverage
  errors now preserve old names while marking coverage unknown. HTTP has a
  connection cap, header size/count/deadline limits, and owned connection drain;
  partial-header clients cannot block shutdown. The launchd repair checks actual
  job removal and recorded PID exit before changing its plist. The expanded
  isolated fixture passes failure, malformed job tables, live-PID drain timeout,
  already-unloaded success, webhook authentication and durable snapshots.
- The native action fixture passes with a private tmux server: absolute config
  selectors, live stdout/stderr logs, durable completion preserving unknown
  metadata, duplicate-run rejection, and child reaping on cancellation. Actions
  and automation evaluation/ledger are implemented as services; application
  dispatch, palettes and source integration remain outstanding.
- The rebuilt checkpoint binary passes cleanup retention and the section-enabled
  PTY fixture again: 1.16 ms navigation during two-second Git work, zero idle
  frames, zero Git scans for a title edit, section filing/renaming, durable writes
  across immediate quit, and clean shutdown. Evidence:
  `/tmp/wt-rust-native-ui-checkpoint-1/result.json`. Incremental-only cache cleanup
  reduced Cargo output from 28 GiB to 13 GiB, preserving compiled output and test
  evidence; multiple compilation profiles still account for the remaining size.
- The native dev fixture passes against a private tmux server and real temporary
  HTTP process: health readiness, capacity refusal, queue timeout/cancellation,
  child and session reaping, failed stop-hook protection, checkout removal, and
  crash output. Cancellation completed in 1.2 ms in this run. Independent
  follow-up review closed all three earlier supervisor/queue findings. Promoted
  waiter ordering is unit-tested; a simultaneous live promotion race is not yet
  exercised.
- Native issue, migration, doctor, fleet and perf commands pass an isolated
  Git/HOME fixture. Migration covers shared legacy JSON, stranded SQLite
  namespaces, archives, unknown fields, current-value precedence, harness
  registries, durable backups and idempotence. Source migration locks now live
  alongside the shared source so different repository lock directories cannot
  authorize conflicting pruning writes.
- SST stage inspection and cleanup pass fake-AWS/pnpm tests, including unknown
  cloud state and a stage claimed by a new worktree between cleanup candidates.
  Unknown, default-personal and foreign stages never reach removal. No live
  AWS mutation was used. Lifecycle removal now shares native stage-pin validation
  and rejects the protected default stage, malformed names and invalid outputs.
- Sections, stack rails, folding, identity-preserving selection and narrow
  picker scrolling pass the 18-test TUI suite and the real-PTY liveness fixture
  (`/tmp/wt-rust-native-ui-sections-1/result.json`). This run measured 1.08 ms
  injected-key-to-output during delayed Git, idle silence and accepted-write
  drain. The subsequent local-source split passes its source regression and
  `/tmp/wt-rust-native-ui-state-lane-1/result.json`: a real title edit starts zero
  Git status scans, while navigation during delayed Git takes 1.12 ms. The source
  test additionally preserves usable state edits and last-good Git facts during
  a failed Git fetch; the error remains visible.
- Section filing, new-section creation, sticky picker targets and folded-header
  renaming now pass controller/model tests. Local and remote layout batches use
  one durable transaction and preserve unrelated fields. The real terminal
  fixture `/tmp/wt-rust-native-ui-filing-2/result.json` verifies filing, neighbor
  selection, Ctrl+D section navigation, rename, zero Git scans for title edits,
  and write-on-quit draining; delayed-Git navigation measured 1.20 ms.
- The native event daemon passes seven crate tests, strict Clippy and an isolated
  binary fixture with fake launchd tools plus a real HTTP listener. Evidence
  covers persistent secrets, foreign-agent ownership refusal, legacy owned
  agent migration, stale-build restart, signed requests, snapshot state, SIGTERM
  and uninstall. TUI cache tests additionally cover an old network reply arriving
  after a newer daemon snapshot and reject stale, incomplete or foreign-build
  cache data. These are fixture checks, not mutation of the user's live daemon.

- Checkpoint `cb34d62` is green in Linux/macOS native tests, optimized builds,
  and strict lint: https://github.com/micthiesen/wt/actions/runs/38003329901.
  The preceding run exposed concurrent first-open contention while changing
  SQLite journal mode. A bounded retry covers that idempotent operation only;
  fresh-database contention, reader release, and deadline tests pass.
- Native installation, update, rollback, declined-build handling, checksum
  rejection and pending-build recovery pass through a loopback release API
  using the real app and launcher (`scripts/native-release-check.py`). It also
  checks that an ordinary configuration error confirms executable health,
  preserves unknown updater fields, and does not roll back. This exposed and
  fixed the launcher's identity-probe newline and macOS PATH symlink handling.
  Hosted release publication and download remain owed.
- `scripts/native-cleanup-check.py` uses five real Git worktrees. It removes
  the merged clean checkout, retains dirty files, an empty branch, unlanded
  commits and an owed post-merge check, retains removal history, and proves a
  repeated sweep is idempotent. Detached lifecycle checks remain green.
- The native worker protocol, snapshot and exact remote argv forwarding passed
  `scripts/native-remote-check.py` through a temporary unprivileged loopback
  OpenSSH server. The fixture owns its keys, configuration and process group.
  This proves native transport without a remote source checkout; remote TUI
  board, cache and action integration remain owed.
- Independent stack/installer review found two races. Force pushes now pin the
  originally observed remote OID even if a concurrent fetch updates shared
  tracking refs. Fork-base edits lock both child and live parent in lexical
  order and revalidate after waiting, preventing assignment to a removed
  parent. Real-Git regressions cover both races and opposite-edge deadlock/
  cycle prevention. Installer migration tests preserve the old checkout and
  archive its user files before replacing a recognized PATH link.

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
