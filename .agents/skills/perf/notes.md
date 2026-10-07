# perf notes — living state

Companion to SKILL.md. Keep current per its §6: refresh the baseline,
track open issues, grow the ledger, prune ruthlessly.

## Baseline (captured 2026-10-06)

Machine: 12 cores / 32 GB (Apple Silicon). The reported freeze was a few
minutes before the user's initial report, approximately 09:13 local
(16:13Z). Historical samples at 16:13–16:14Z showed load 17–18,
Vitest 228–260% CPU, wt 94–129%, native pg_cron ~100%, Brave ~100%, and
WindowServer ~50%. These are ps decaying averages, not instantaneous
measurements or proof of which process blocked the desktop.

Later samples had 42–52% CPU idle and stable 350 MB swap use, with no
swap/pageout/compression growth over eight seconds. This quieter baseline
does not disprove the earlier freeze. Small old Supabase Docker containers
used roughly 350 MB and less than 1% CPU; the hot database was native.
Brave's background thread still consumed a core. The wt main-thread
sample was mostly idle while a worker periodically became busy.

Shared Codex daemon children can fall outside wt's process ancestry even
when the work began in a wt session. Correlate cwd, session history, and
command times before assigning all "not downstream" processes to other
apps. RSS is not macOS physical footprint; wt measured ~209 MB footprint
despite roughly 900 MB RSS.

## Open issues

- **Codex updater ownership and drain exits (2026-10-07).** move-files-to-r2
  exited at 09:56:51 local after `turn/steer failed: Server is draining;
  retry after reconnecting`. Native logs confirm repeated shutdown-signal
  restarts, but not the signal sender. Six same-user update loops coexist;
  three independently logged scheduled updates. Dotfiles c32b1d8 only fixed
  daemon argv recognition. A singleton repair preserving the current daemon
  and native TUI reconnect handling remain owed; no updaters were stopped.
  Evidence: `/tmp/wt-daemon-followup-20261007/assessment.md`. Feature-list
  responsiveness does not prove that a draining server admits new turns.
- **Native Supabase cron startup remains broken.** CLI 2.119.0's native
  Postgres for set-your-status exposed only a Unix socket, while cron
  connected to localhost:5432. It accumulated 18,115 failed attempts and
  almost 15 CPU-hours. Reloading `cron.launch_active_jobs=off` stopped the
  loop without changing the 67 jobs or 23 active flags; CPU time and
  attempt count stayed unchanged over 61 seconds. The worktree was later
  removed and its original processes were gone. Retained stack metadata
  is not evidence of an orphan. Future/restored stacks still need a
  functional startup fix (upstream Supabase CLI issue #6977), including
  database-test startup. Background-worker cron requires a controlled
  restart and functional verification. No Cozee source change was made.
- **Brave background CPU and severe desktop freezes remain unexplained.**
  PID 57049 consumed about one core with its AppKit thread idle in a native
  sample. No browser state was changed. Reduced test concurrency and wt
  scan work remove demonstrated pressure; they do not prove elimination
  of every freeze or Codex's native feature-discovery timeout.
- **Destroy dispatch double-fetches GitHub** — two concurrent
  `fetching GitHub...` ~40ms apart (double invalidation while the first
  is in flight). Harmless, minor quota waste. Found in dogfood sweep
  (FINDINGS.md), still unfixed.

## Learnings ledger

Failure signatures (check these first):

- **Same-version daemon feature mismatch can be a desktop runtime default.**
  On 0.161.0, the shared daemon reported `api_key_model_discovery=false`
  although the CLI default was true. The desktop rollout gate can set that
  runtime value without a config-file change. Dotfiles adc455c explicitly
  enables it in shared config, which takes precedence over host defaults.
  Readback matched all four native compatibility features without restarting
  PID 41633. A fresh TUI reached `/status: Local background server` in 1.877s;
  this fixes the mismatch, not the separate drain exits or historical timeouts.
- **Obsolete test caps silently allow all-core parallelism (dotfiles 9cb5756).** Vitest 5
  honors `VITEST_MAX_WORKERS`, not the old fork/thread env limits. Here
  it defaulted to 11 workers. set-your-status's 394.5s native typecheck
  overlapped an uncapped full suite after a review prompt demanded both.
  The later two-worker run passed 6,613 tests in 219.37s, but is not a
  controlled speed comparison because typechecking had finished. The
  agent Node preload now defaults modern workers and `GOMAXPROCS` to 2,
  preserves explicit choices, and applies to existing sessions on their
  next Node command. Real Vitest worker counts and native Go scheduler
  traces verified inheritance. Direct native commands bypassing Node
  still need their own limits. User wt actions run checks serially and
  the review action reuses relevant completed checks instead of always
  requesting another full suite.
- **Excluded histories still cost reads unless exclusions are cached (9258ae0).**
  Codex guardian/subagent rollouts failed the interactive filter and were
  reread for every slot on every scan. On 1,684 real rollouts, a warm scan
  read 1,480 excluded 64 KiB prefixes (~93 MB requested) in 270.78ms.
  Caching complete exclusions against size, mtime, ctime, inode and device
  reduced that to zero reads and 27.33ms. Cold scans were unchanged.
  Keep this bounded and retry changed/incomplete/unrecognized headers;
  caching a partial first line as a permanent rejection hides sessions.
- **Removal has a separate reclamation phase (9258ae0).** `rift remove` moves a
  clone to trash; immediate `rift gc` physically deletes its files. The
  16:46Z facebook-status removal coincided with a transient Rift/FSEventsd
  CPU spike, not proof that the filesystem watcher or that deletion caused
  a desktop freeze. GC now has separate phase timing and requests nice 10
  plus macOS background scheduling when available. It remains awaited.
  Verify scheduling in an isolated child; do not delete live worktrees to
  reproduce load. An OS priority request does not guarantee zero latency.
- **Bun spins at 100% on a bare pending promise.** `await new
  Promise(() => {})` with no other event-loop handle makes Bun busy-spin
  instead of block (bun 1.3.14; 19h CPU burned in `wt _home` once —
  fixed 4e18459 with an inert `setInterval`). Signature: one
  wt-category process pinned at ~100% while functionally idle. Applies
  to any `_`-prefixed entrypoint meant to just sit there (also a
  CLAUDE.md trap).
- **Loop stalls corrupt DATA, not just latency.** A blocked event loop
  makes any timeout-vs-IO race resolve the wrong way (libuv runs timers
  before poll), so a stall shows up as a wrong answer somewhere else
  entirely. The dev-server bolt vanishing off rows "when lots is
  happening" was this: a 400ms socket timeout beating a `connect` that
  had already succeeded, reporting a live server as dead. Signature: a
  correctness symptom that only appears under load and clears on `r`.
  When you get one, look for a deadline racing an IO callback before
  looking for a logic bug — and check `grep 'event-loop blocked'` for
  whether stalls exceed that deadline (574ms is on record).
- **Leaked headless wt instances.** A dead terminal can orphan the TUI
  to launchd (pre-SIGHUP-handler builds, wedged teardowns); each orphan
  keeps polling GitHub and duplicating attention lines. One sweep found
  33. `wt perf` hunts these itself — LEAKED section with a ready `kill`
  line. Propose the kill, don't run it unasked.
  **Confirm what the pid IS before proposing it**, because ppid 1 means
  two opposite things: a TUI that lost its terminal, and a daemon
  launchd is supervising exactly as designed. `wt events serve` is
  long-lived, headless, parented to launchd and talks to GitHub, i.e.
  every surface marker of a leak. It was reported as one, with a kill
  line, against a daemon the user had deliberately installed the day
  before. The sampler now excludes pids launchd claims under a
  `com.wt.*` label (asked at sample time, not a remembered list of
  daemon names), so a clean orphan list is trustworthy again — but
  `launchctl list <label>` and `ps -o ppid,command` are two seconds and
  settle it either way.
- **Heavy parsing on the render thread.** Codex-events JSONL tailing
  used to block the TUI's single JS thread; moved to a worker in
  61634bc (`core/harness/codex-events-worker.ts`). If the loop-lag
  probe shows blocks correlated with a data source, suspect synchronous
  parsing and reach for the same worker pattern.
  Codex historical-session discovery follows the same rule: selecting a
  row starts discovery for F12, and its 30-day rollout walk must stay in
  `core/harness/codex/discovery-worker.ts`. The main-side client serializes
  scans and drops cancelled queued destinations during rapid j/k input.
  The detailed live-output tail is also worker-owned
  (`core/harness/codex/tail-worker.ts`): its 2.5s poll used to perform the
  same tree walk on the UI thread, producing intermittent input stalls.
  2026-08-15 real-fleet probe: one discovery scan took 106.2ms wall time
  while the main-thread 5ms timer saw only 0.8ms max lag; the old direct
  rollout lookup measured 25–107ms per worktree / 355ms over ten rows.

- **Permanent live-mode rendering (RESOLVED 2026-08-11, f54910e…0094b27).**
  OpenTUI Timelines held a renderer-wide live request: continuous
  ~60fps full-tree walk + full repaint, `requestRender()` a no-op (so
  keypresses couldn't pull frames forward), ~13% idle CPU per
  instance, cost scaling with board size — the root of "j/k laggy
  when lots is happening". Fixed as a six-part series: shared
  refcounted 100ms ticker replacing all Timelines (f54910e, idle
  0.1%); `useIsFetching` isolated into a memoized TitleBar +
  `React.memo` on WorktreeList/Details (a152a9a); the events feed
  renders a 120-event window behind an exact-height spacer (d6def0c);
  keypress→frame histogram + live-duty instrumentation (e89602b);
  claude jsonl tailing in a worker + per-key registry selectors
  (c832d3c); @opentui 0.1.102 → 0.5.1, native yoga (0094b27). The
  invariants live in docs/architecture.md#rendering--input-latency
  and the Timeline trap in CLAUDE.md. Signature if it regresses:
  idle TUI CPU >5%, or WT_PERF's `input-latency` line showing
  liveDutyPct > 0.
- **Whole-App re-render per fetch event** (fixed a152a9a): keep
  global in-flight counters inside a small leaf component (title
  bar), never at the root — `useIsFetching()` re-renders per fetch
  start/finish anywhere.
- Upstream opentui #1339 (per-frame O(tree) walk) is still open even
  at 0.5.1 — renderable-tree size stays a per-frame tax regardless of
  version; window unbounded buffers.

- **Post-sweep stall (`c`) — RESOLVED 2026-08-13, three parts.** The
  render thread blocked in multi-SECOND chunks after a clean sweep
  (sealed fixture, 28 rows, 7 candidates: 4104 / 4209 / 2650ms back to
  back, ~12s of a 14s window; the input-latency probe logged n=2 for
  that minute because keypresses never reached a painted frame). Not
  rendering — three separate causes stacked, each measured alone:
  (1) the O(N²) tracked-props combine below → worst block 507ms;
  (2) `doCleanRows` ending in `refreshAll`, whose `["wt"]` wave refetches
  every field of every row — replaced with a scoped
  `refreshAfterRemoval` (list + wtState), since a destroy changes which
  worktrees exist, not the survivors' state;
  (3) `RUN_CONCURRENCY` in `core/proc.ts` capping concurrent `run()`
  subprocesses, because `Bun.spawn`'s `posix_spawn` is synchronous on the
  calling thread. (2) and (3) address the SAME burst from opposite ends,
  so (3) shows no gain on the sweep once (2) lands — its win is on the
  bursts (2) can't remove, i.e. pressing `r`: worst block 2699ms → 185ms
  on a 22-row board. End state on the sweep: worst block ~400ms with
  input staying live throughout (p50 8ms, n=99/215 samples per minute vs
  n=2 before).
- **`useQueries` + `combine` is O(N²) per query update.** query-core's
  `QueriesObserver.#trackResult` wraps every result in a tracked-props
  Proxy whose `onPropTracked` callback loops over ALL observers in the
  batch — so one property read inside `combine` costs N `trackProp`
  calls, and a combine reading P props over N queries costs N×P×N. It
  re-runs on EVERY query update in the batch. `useWorktreeRows` puts
  worktrees × 10 fields in one batch: at 28 rows that's 280 queries ×
  5 props × 280 = 392k `trackProp` calls per update, and a `c` sweep
  fires hundreds of updates. Signature: a `bun:jsc` sampling profile
  where `trackProp @ queryObserver.js` is >50% SELF time, under
  `#combineResult` → `performProxyObjectGet`. The escape hatch is
  declaring `notifyOnChangeProps` on the queries — query-core then skips
  the proxy entirely (`!match.defaultedQueryOptions.notifyOnChangeProps`
  is the branch). It is quadratic in BOARD SIZE, so it degrades as the
  fleet grows and is invisible on a small one.
- **React was not the culprit and a React Profiler proved it in one
  run.** Wrapping the root in `<Profiler onRender>` during a 3.6s block
  showed 21 commits totaling 59ms. Do this before chasing render cost —
  it separates "the tree is expensive" from "something else owns the
  thread" for the price of five lines.
- **`sample <pid>` can't symbolicate JIT frames; `bun:jsc` can.**
  `sample` shows the main thread deep in unnamed `??? (in bun)` frames,
  which is only enough to rule out native work. `import {
  startSamplingProfiler, samplingProfilerStackTraces } from "bun:jsc"`,
  dump on SIGUSR2, and you get named JS frames with source URLs —
  that is what named `trackProp` above. Works on a live TUI, unlike
  `--cpu-prof`.
- **fs.watch on macOS coalesces deletes; it is not a storm.** Deleting a
  20k-file tree under a `recursive: true` watch delivered 233 events and
  3.4ms of callback time total. Rule out the watcher hypothesis with a
  10-line standalone script before designing around it.
- **A heavy sealed fixture reproduces this in ~2 minutes.**
  `scripts/fixture.sh build`, then add landed rows carrying an
  APFS-cloned (`cp -c -R`) 30k-file `node_modules` (gitignore it via
  `main-clone/.git/info/exclude`, or the rows read dirty and the sweep
  keeps them), arm `WT_PERF` on the probe server
  (`tmux -L wt-tui-test set-environment -g WT_PERF 1`) and press `c`.

Measurement traps:

- The loop-lag probe's 50ms threshold misses 16-50ms blocks — one to
  three dropped frames each, exactly the range that makes input feel
  laggy. For latency work, measure end-to-end instead: send a key via
  tmux, poll `capture-pane -e` for the selected-row bg SGR
  (`48;2;59;66;82`) to move (scripts from the 2026-08-11 session:
  `/tmp/wt-perf-jk-latency.pl`, churn generator
  `/tmp/wt-perf-fixture-churn.sh` — recreate from git history of this
  note's session if reaped). Fixture baselines at 110x30: j ~34ms /
  k ~22ms median incl. ~10ms of measurement overhead; under
  file-touch churn: TUI CPU 13%→81%, 9 loop blocks >50ms in ~2min,
  p90 ~50ms.
- OpenTUI has native instrumentation: `OTUI_SHOW_STATS=1` (frame-time
  overlay), `OTUI_TRACE_FFI=1` (per-call FFI trace),
  `renderer.getStats()` after `setGatherStats(true)`. The 0.1.102 FPS
  counter is unreliable (fixed upstream 0.4.2, "stale fps"); trust
  frame times + `ps` over it. Frame times exclude the threaded native
  write.
- `bun --cpu-prof` writes nothing when the app exits via
  `process.exit()` (main.ts does) — use macOS `sample <pid>` for
  native-side confirmation instead.
- tmux probe env: `tui-test.sh start` passes only an allowlist of WT_*
  vars, and an already-running probe server won't inherit fresh
  exports — `tmux -L wt-tui-test set-environment -g NAME val` before
  `start` is the reliable way to arm WT_PERF / OTUI_* on a probe.

- `ps` `%CPU` is a lifetime decaying average — a 2s-old process is
  barely averaged, a long-lived one remembers old load. `top -l 2`'s
  second sample is instantaneous; use it to disambiguate.
- `os.freemem()` counts only genuinely-free pages (~90% "used" always);
  vm_stat active+wired+compressor is the honest number (`wt perf` does
  this already).
- The idle TUI intentionally does NOT re-render (structural sharing on
  unchanged refetches). Anything time-derived computed at render time
  freezes — the details pane has a 30s tick for exactly this (50d50c3).
  Don't "fix" idle-freeze symptoms by adding polling; add a tick.

Design rules with perf teeth (from CLAUDE.md, restated here because
perf work is where they get bent):

- The GitHub source batches fixed-size chunks to stay below the server's
  execution-time ceiling; never fetch per row. New PR fields go into
  `PR_FRAGMENT`.
- Freshness is push-based; never shorten a staleTime to paper over a
  missing invalidation trigger.
- Perf sampling itself is free when idle: the `P` overlay samples only
  while open, `wt perf` is one-shot, nothing persists to the query
  cache. The loop-lag probe (WT_PERF=1) costs one 100ms interval.

Tooling inventory:

- `wt perf` / `wt perf --json` — one-shot snapshot, agent-friendly
  (added 31734d2; also roots at live wt instances so the TUI counts as
  "us" from the CLI).
- TUI `P` overlay — same sampler, 2s cadence while open; `i` injects
  the report into the wt-source session.
- `WT_PERF=1` loop-lag probe — 100ms sample, warns at >20ms block,
  `grep 'event-loop blocked'` in the daily log. Startup-only.
- `WT_PERF=1` input-latency probe (same arming) — keypress→painted-
  frame histogram, one `input-latency` INFO line per minute
  (p50/p90/max + liveDutyPct), immediate warn on any >100ms sample.
  Samples close on the renderer's post-paint `"frame"` EVENT — a
  frame CALLBACK runs before layout+paint and would exclude exactly
  the board-size-dependent cost (an early probe build did; its 4/6ms
  figures were pre-paint). Fixture baseline, corrected instrument:
  idle p50 5ms / p90 6ms; under file-touch churn p50 6ms / p90 19ms /
  max 20ms; duty 0. TUI CPU: idle ~0-1% (was ~13%), churn ~4%
  (was 81%).
- `wt-state` skill — read-only cache/tmux/log/lock inspection.
- `scripts/tui-test.sh` — probe harness for reproducing TUI-side
  behavior without touching the live instance (read-only rules apply).
