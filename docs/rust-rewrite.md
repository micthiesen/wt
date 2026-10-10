# Native Rust rewrite execution record

The production TypeScript application has been retired from this branch. Its
behavior baseline is commit `d9cd2f4`; that checkout is a historical oracle,
not a runtime, build, or test dependency of the native application. The original
pre-promotion checkpoint was `e122c3d`; its final Linux/macOS CI passed. The
subsequently authorized cutover promoted that commit to main.

The scope and evidence are summarized in
[rust-rewrite-inventory.md](rust-rewrite-inventory.md). This record distinguishes
the verified application build from the final test, documentation, and CI setup
follow-ups.

## Current state

The first live cutover found an unbalanced selection in the GitHub PR query.
The missing closing brace is corrected, generated-query regression assertions
cover queue and non-queue requests, and the corrected query succeeded against
the live repository. The bundled wt skill now documents the native PATH entry.
These follow-ups are included before the first stable publication.

Application build `4ef9878` passed all 577 workspace tests, doctests,
formatting, strict Clippy, and dependency checks
(`/tmp/wt-rust-final-retired-gate.log`). Linux and macOS
[Rust CI 38023495258](https://github.com/micthiesen/wt/actions/runs/38023495258)
passed. The four-target
[release run 38023512727](https://github.com/micthiesen/wt/actions/runs/38023512727)
passed on macOS arm64/x86_64 and Linux arm64/x86_64 (glibc, Ubuntu 22.04
baseline), publishing the isolated `rust-test-4ef9878-20261010` release.
Its published bootstrap matched the source installer. An isolated HOME and
install root verified installation without PATH changes, explicit legacy-link
migration with checkout backup, updates between two published releases,
rollback, and an ordinary command startup confirming the final build:
`/tmp/wt-native-release-4ef9878-3/result.json` and
`/tmp/wt-native-release-4ef9878-check-3.log`. The user's real PATH link was
unchanged. Informational `version` and boot-probe calls deliberately do not
confirm a pending application boot; the command fixture exercises that boundary.

The same installed controller provisioned its exact Linux build on Boris,
verified selected configs and PATH in managed sessions, literal shell quoting,
and two independent configs using the same slug. An unrelated sentinel stayed
alive. Cleanup removed the private fixture and newly provisioned unused
runtime: `/tmp/wt-native-boris-4ef9878/result.json` and `cleanup.json`.

The final branch CI exposed a test timing assumption after that release:
macOS could deliver a setup file-write event during an assertion that a
neighboring file produced no event. The existing callback was extracted
unchanged and its filtering/coalescing policy is now tested with deterministic
events. Independent review confirmed identical runtime behavior. The real
filesystem notification test and post-warmup output fixture remain. Release and
performance figures identify `4ef9878`; later changes consist of this testability
refactor, documentation, and a quiet Homebrew dependency-presence probe.

Independent review covered the shared contracts, durability, remote protocol,
distribution, and full feature inventory, followed by a final review of the
presentation pipeline, installer, stage identity, and removal proof. It found
and corrected delayed Git facts overwriting current inventory, weak landing
proof, hidden probe failures, and lost combined-host section summaries. A final
cache-invalidation refinement passed independent re-review. No accepted finding
remains open. The final PTY checks passed normal
quit and SIGTERM, including navigation during a two-second Git stall (1.13 ms
key-to-output sample), idle rendering, external edits, metadata-only writes,
sections, feeds, history, and accepted-write draining. Evidence lives under
`/tmp/wt-native-ui-retired-final` and `/tmp/wt-native-signal-retired-final`.
The retired-source CLI fixture and all nine old/new JSON comparisons passed
(`/tmp/wt-cli-compat-retired-final/result.json`).

## Proof by surface

- **Commands and durable data:** `native-command-check.py` covers migration,
  diagnostics, issue operations, and selected `ls`, `fleet`, `perf`, and
  version behavior. Migration preserves unknown state and backups and is
  idempotent. The nine representative compatibility comparisons pass. This
  evidence is representative, not a full snapshot of every output string or
  failure branch.
- **Lifecycle and cleanup:** native lifecycle, cleanup, and resource fixtures
  use isolated real Git worktrees and private tmux state. They cover guarded
  create/remove, revision checks, dirty/unlanded/owed-verification retention,
  history, idempotence, and exact resource scope.
- **TUI and sessions:** isolated PTY fixtures cover delayed Git navigation,
  file refresh, accepted writes draining on quit, section/history/feed slices,
  and tmux detach/resume. The current presentation and source changes passed
  the final gate and PTY pass described above.
- **Integrations:** focused crates cover GitHub batching and ambiguous writes,
  SST unknown-state safety, event authentication/ownership, automation claims,
  action logs, and dev-server supervision. Local fake services prove only the
  behaviors each fixture asserts.
- **Remote and release:** loopback SSH/host fixtures prove protocol framing,
  exact argv, build checks, reconnect and isolation. The Boris fixture adds
  real cross-platform provisioning. Release fixtures use optimized binaries
  for checksum validation, install/update/rollback, candidate probing and
  recovery.

## Performance evidence

The optimized native build improved all five workloads on the same Apple M2
Pro (12 cores, 32 GiB, macOS 26.6.2). Each used 24 rows, a 180×50 terminal,
65 seconds, and a five-second warmup before the key stream. CPU is a percentage
of one core, including startup and waited subprocesses such as Git.

| Workload | TypeScript CPU | Rust CPU | CPU reduction | TypeScript / Rust peak RSS |
|---|---:|---:|---:|---:|
| Idle board | 8.11% | 3.11% | 62% | 288 / 23 MiB |
| Navigation | 31.09% | 4.09% | 87% | 344 / 23 MiB |
| Repeated refresh | 71.58% | 8.66% | 88% | 386 / 23 MiB |
| Create/remove during use | 14.76% | 4.56% | 69% | 348 / 24 MiB |
| Active agent output | 11.16% | 4.94% | 56% | 309 / 25 MiB |

A separate warm idle measurement of the UI process alone, excluding Git
children, fell from **1.54% to 0.12%** of one core. It uses cumulative
`ps TIME` deltas over 50 seconds after five seconds warmup: 0.77 versus
0.06 CPU seconds, with 0.01-second reporting precision. Evidence:
`/tmp/wt-idle-self-cpu-20261010.json`.

Navigation's internal input-receipt-to-draw p90 fell from **12 ms** (638
samples) to **1.24 ms** (637 samples). The final native reporting window added
55 samples at p90 1.23 ms; percentile windows are kept separate. The maximum
was 110 ms for TypeScript and 1.58 ms for Rust. These instruments end at
terminal draw completion and exclude physical display timing. The separate
delayed-Git PTY fixture stayed responsive with a 1.13 ms key-to-output sample.

The active-output fixture wrote 264 synthetic Codex records at five per second
through a live private tmux session. Its first post-warmup marker reached the
selected pane in 39 ms versus 590 ms, proving that the tail stayed subscribed.
That is one visibility sample, not a tail-latency distribution. Lifecycle CLI
work is accounted separately: 0.483 CPU seconds / 0.531 elapsed seconds native,
versus 0.714 / 0.556 for TypeScript. It creates and removes a worktree without
dependency installation, cloud deployment, or an external agent turn.

GitHub, automations, AI naming, updates, and skills were disabled. No Cargo
build or test suite overlapped these runs. RSS is OS-reported peak resident
memory, not macOS physical footprint or a sum across the process tree. This is
one run per workload and implementation; it establishes improvements for these
workloads, not a confidence interval or a cure for unrelated desktop freezes.

The TypeScript runtime fixture used `bbf1696` plus a two-token tmux delimiter
fix: tmux 3.9 and official 3.7c emit `_` for a tab delimiter. Both sides used
tmux 3.7c. Original language-neutral goldens remain tied to `d9cd2f4`.
Reproduce with `scripts/perf-baseline.py --rows 24 --seconds 65 --scenarios
idle,navigation,refresh,lifecycle,codex_output --ui native -- target/release/wt`
and a unique `--output` directory; use `--ui legacy -- bun <baseline>/src/main.ts`
for the oracle. Build native with `cargo build -p wt-app --release --locked`
before starting the timed run.

Full metrics are retained in [rust-rewrite-performance.json](rust-rewrite-performance.json).
Raw data and terminal captures are under `/tmp/wt-perf-ts-final-24-20261010`
and `/tmp/wt-perf-rust-final-24-20261010`. Native input-latency windows are in
the latter's `cache/logs/app/wt-native.2026-10-10.log`.

## Acceptance and limits

Application implementation, independent review, compatibility fixtures,
performance comparison, hosted CI, and native release/remote checks are
complete. The TypeScript production tree and manifests have been removed;
project instructions and feature docs describe the native implementation.
The feature ledger accounts for deliberate behavior changes and evidence
limits. Live AWS deletion and successful delivery through every installed
agent CLI were not exercised; their adapters and safety rules have isolated
tests. Native Windows is unsupported; WSL is not claimed as tested.

Builds use bounded local concurrency, small development symbols, disabled CI
incremental compilation, separate lint/test/release caches, and main-only
release-cache writes. After validation, clearing accumulated debug artifacts
reclaimed 34.4 GiB, leaving 1.3 GiB of optimized outputs. Test logs and workload
evidence were retained. Publication checks below are tracked separately from
the pre-promotion evidence.

## Promotion and recovery

The cutover follows this order and retains the recovery copies.

1. Before updating an existing TypeScript source installation to native main,
   close its boards and stop its owned events daemon. Leave agent tmux sessions
   and worktrees running. Install the tested `rust-test-4ef9878-20261010`
   release with its published `install.sh --release rust-test-4ef9878-20261010
   --path`. The installer archives the recognized old checkout before switching
   the PATH link; it leaves the checkout in place.
2. For each selected repository config, import legacy state with
   `wt state migrate --keep-legacy`, inspect `wt ls --json` and
   `wt status --all --json`, and open the native board. Use `WT_CONFIG` and
   `--from` explicitly where appropriate. Repeat on worker hosts with their
   worker configs. See [the migration procedure](updates.md#moving-an-existing-source-installation-to-native-wt).
3. Reinstall and start an enabled events service under its owning config.
   It must invoke the stable native launcher. Keep config selectors, the old
   checkout archive, legacy JSON backups, and SQLite state intact.
4. Promote the approved branch. A push to main publishes a checks-gated
   `preview-<full-main-sha>` release. Confirm all four assets and the manifest
   are present, then use `wt update --channel preview` in an isolated install
   and check that `wt version` reports that main SHA.
5. For the first stable release, tag the approved main commit `v0.1.0` and let
   the same four-target workflow publish it. No stable tag existed at this
   checkpoint. Verify the public latest installer in a fresh isolated HOME
   and install root, then `wt update --channel stable --check`. The default
   bootstrap URL requires that first stable release; test prereleases are
   intentionally excluded from stable and preview discovery.

The main push trigger, first stable publication, latest-stable bootstrap URL,
and real channel discovery can only be verified after those publications.
Their packaging, selection, install, update, and failure-recovery code is
exercised before promotion through CI fixtures and isolated test releases.

For a bad later native release, `wt rollback <previous-release-tag>` activates
an already installed version; a failed pending boot also selects the prior
fallback. For the first native cutover, TypeScript recovery uses the preserved
checkout and legacy data. Do not overwrite new SQLite edits with old JSON:
native rollback does not export state back to TypeScript. Existing tmux agents
are independent of the board and remain running across native updates.
