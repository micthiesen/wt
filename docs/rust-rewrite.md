# Native Rust rewrite execution record

The production TypeScript application has been retired from this branch. Its
behavior baseline is commit `d9cd2f4`; that checkout is a historical oracle,
not a runtime, build, or test dependency of the native application. Promotion
from `rusty` to `main` is not authorized.

The scope and evidence are summarized in
[rust-rewrite-inventory.md](rust-rewrite-inventory.md). This record distinguishes
the accepted native checkpoint from later shared-tree changes.

## Current state

Checkpoint `7ab434d` passed the 547-test workspace gate, doctests, formatting,
strict Clippy, and dependency checks (`/tmp/wt-rust-gate-review-fixes.log`).
Rust CI run
[38020225318](https://github.com/micthiesen/wt/actions/runs/38020225318) and
the four-target native release run
[38020250780](https://github.com/micthiesen/wt/actions/runs/38020250780) are
green. The latter published the isolated
`rust-test-7ab434d-20261010` release.

That release installed from the isolated bootstrap flow
(`/tmp/wt-native-release-7ab434d-install.log`). A real Boris SSH run provisioned
the exact matching Linux binary from macOS, verified selected config and PATH
in managed sessions, preserved shell quoting and `$HOME`, used two configs
with the same slug, and left a sentinel alive after fixture cleanup:
`/tmp/wt-native-boris-7ab434d-3/result.json` and `cleanup.json`. Nine
representative CLI comparisons passed at
`/tmp/wt-cli-compat-final-fixed/result.json`.

The retired-source checkout passed an expanded 577-test workspace gate
(`/tmp/wt-rust-final-presentation-gate.log`). Independent review covered the
presentation pipeline, installer, stage identity, and removal proof. It found
and corrected delayed Git facts overwriting current inventory, weak landing
proof, hidden probe failures, and lost combined-host section summaries. A final
cache-invalidation refinement also passed the full 577-test gate
(`/tmp/wt-rust-final-retired-gate.log`). The final PTY checks passed normal
quit and SIGTERM, including navigation during a two-second Git stall (1.13 ms
key-to-output sample), idle rendering, external edits, metadata-only writes,
sections, feeds, history, and accepted-write draining. Evidence lives under
`/tmp/wt-native-ui-retired-final` and `/tmp/wt-native-signal-retired-final`.
The retired-source CLI fixture and all nine old/new JSON comparisons passed
(`/tmp/wt-cli-compat-retired-final/result.json`). The hosted 7ab434d runs do not
cover these later changes.

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

The TypeScript runtime fixture is complete: five 65-second scenarios with a
24-row board, five seconds warmup, and optional integrations and sessions
disabled. It used checkout `bbf1696` plus a two-token tmux delimiter fix because
tmux 3.9 and official 3.7c emit `_` for a tab delimiter. Results:
`/tmp/wt-perf-ts-final-24-20261010/results.json`. Original language-neutral
goldens remain from `d9cd2f4`. The matching optimized Rust comparison is still
owed. Until it is run, the rewrite has no accepted claim of CPU or latency
improvement.

Earlier investigations measured a 480-Git-launch refresh, a 4.6-second
synchronous copy, and a 4.62-second worst timer gap before targeted fixes.
Those historical values explain separate throughput and responsiveness
measurements. Earlier native single-sample PTY readings near one millisecond
are not a latency distribution or an optimized workload comparison. See
`.agents/skills/perf/notes.md` for historical measurements and limits.

## Remaining work

1. Run the optimized Rust workload against the `bbf1696` TypeScript fixture
   with the same tmux delimiter fix; report process-tree CPU/RSS, idle TUI CPU,
   and injected-key-to-painted-frame latency separately.
2. Rerun required current-head Linux/macOS CI and release checks after shared
   changes settle. The 7ab434d green run does not cover them.

The native rewrite is not yet declared complete. This record authorizes no
promotion and does not claim any post-promotion verification.
