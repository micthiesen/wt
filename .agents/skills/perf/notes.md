# Performance notes

Companion to `SKILL.md`. Keep the baseline and open issues current; preserve
measured evidence, and prune implementation-specific advice when it no longer
applies.

## Live machine baseline (2026-10-13 UTC)

### COZ-2919 creation report (2026-10-14 UTC)

The reported desktop freeze during `coz-2919-task-e87934a320` creation was
not sampled while active. At 16:41:40Z its tracker action completed; subsequent
TUI draw maxima were 2.48 ms and 1.51 ms, which exclude desktop input transport
and cannot disprove the freeze. At 16:45Z, the second `top` sample was 65.2%
idle with wt at 9.5% of one core, Brave at 66.8%, and no swapout increase.
`wt perf` reported wt's process tree as 20.4% of one core, 3% of CPU in use;
the decaying snapshot overlapped a Cargo build and is not a steady baseline.

Installed Rift is npm `rift-snapshot` 0.0.10. Its APFS `CopyMode::All` calls
one `clonefile(source_directory, destination, 0)`, not a userspace recursive
copy. Filtered mode instead walks entries and clones individual files. Keep
`--copy-all`; removing it changes dependencies and is not a performance repair.
Rift then writes its marker/HEAD/registry and runs optional postcreate hooks;
the inspected main clone had no `.rift.toml`. wt subsequently refreshes the
copied Git index and switches branches. No evidence yet isolates kernel clone,
Git index I/O, or filesystem watchers as the freeze cause.

New `wt::creation` stage timings distinguish these command boundaries without
logging arguments or output. Capture process stacks and machine pressure during
the next slow create before changing scheduling, cloning strategy, or Rift
versions. Source: anomalyco/rift tag v0.0.10, `crates/core/src/strategy/apfs.rs`
and `crates/core/src/lib.rs`. No production clone, process stop, or priority
change was performed for this investigation.

The final optimized `b9d8d2f` candidate used 12.78% of one core over 60 seconds
after 15 seconds warmup, with the real Cozee configuration, local and managed
MaxBook sources, and no features disabled. Quit after an explicit refresh took
0.0068 seconds. This measures the UI process only, excluding subprocess CPU;
other live work was uncontrolled. Evidence:
`/tmp/wt-live-final-b9d8d2f/result.json`.

The installed v0.1.13 immutable runtime independently measured 12.80% over
60 seconds and quit after refresh in 0.0043 seconds, exit 0, under the same
full configuration (`/tmp/wt-live-installed-v013-runtime/result.json`). An
earlier sample of the stable launcher measured its waiting parent rather than
the UI child and was discarded; profile the runtime or account for descendants.

A separate final second `top` sample showed 87.66% machine idle and no swapout
increase. Brave PID 83591 still used 90.5% of one core. The two inspected local
cron launchers used 0%; their scheduled execution remains paused, not repaired.
Evidence: `/tmp/wt-final-machine-load-20261014.txt` (captured October 13 UTC).

## Overlapping removal and quit (2026-10-13 UTC)

On debug candidate `36be59a`, an isolated PTY fixture held one durable removal
inside its destroy hook. The second worktree's delete confirmation appeared in
87.8 ms, and quitting took 22.1 ms while the first hook remained blocked. After
exit, releasing the hook let the worker finish and persist success; the second
worktree stayed intact. Evidence: `/tmp/wt-held-removal-ui-36be59a-v2/result.json`.
These are single local samples with optional sources disabled and other native
checks running, not remote latency or CPU measurements. The previous multi-minute
quit was waiting for an independently owned removal; PID/start-identity proof
now permits detaching that wait without cancelling the worker. Menu preparation
has separate bounded capacity through both the controller and remote transport.

## Controlled native comparison (2026-10-10 UTC)

On the same Apple M2 Pro (12 cores, 32 GiB, macOS 26.6.2), the optimized
`4ef9878` binary improved all five 24-row, 65-second workloads against
TypeScript `bbf1696` with the minimal tmux delimiter repair. Process-tree CPU
as a percentage of one core was: idle 8.11 → 3.11, navigation 31.09 → 4.09,
refresh 71.58 → 8.66, lifecycle 14.76 → 4.56, and active Codex output
11.16 → 4.94. Peak RSS was 23–25 MiB for Rust versus 288–386 MiB for
TypeScript. CPU includes waited Git children and startup; the synthetic writer,
driver, and lifecycle CLI are accounted separately. RSS is not physical
footprint or a sum of the process tree.

A separate warm idle UI-only measurement excluded Git children: 1.54% of one
core for TypeScript versus 0.12% for Rust. It sampled cumulative `ps TIME`
over 50 seconds after five seconds warmup (0.77 versus 0.06 CPU seconds,
0.01-second reporting precision). Evidence: `/tmp/wt-idle-self-cpu-20261010.json`.

Navigation's input-receipt-to-draw p90 was 1.24 ms in the main native window
(637 samples), versus 12 ms (638 samples). The native final window contained
55 additional samples with p90 1.23 ms; these percentiles are not averaged.
Normal and SIGTERM PTY checks also proved no idle frames, metadata-only title
writes without Git rescans, accepted-write draining, and navigation during a
two-second Git stall. The first post-warmup agent-output marker appeared in
39 ms versus 590 ms; that is one sample, not a tail-latency distribution.

Optional GitHub, automation, naming, update, and skills sources were disabled.
No Cargo build or test suite overlapped the measurements. This isolates the
board, Git, lifecycle, and transcript costs; it does not prove elimination of
the unrelated desktop freezes below. Reproduction and full measurements are
in `docs/rust-rewrite.md` and `docs/rust-rewrite-performance.json`. Raw runs:
`/tmp/wt-perf-ts-final-24-20261010` and
`/tmp/wt-perf-rust-final-24-20261010`.

A lifecycle feedback fixture on the debug build showed the pending creation
row in 35.5 ms while inventory was held for two seconds. Checkout completion
was reported before an eight-second dependency install finished. A separate
PTY section check drew navigation in 1.2 ms during a two-second Git stall.
These are single local samples with optional sources disabled, not remote
latency guarantees. The native lifecycle check also verified archive rollback
when a worker refused a changed HEAD. Evidence:
`/tmp/wt-create-feedback-20261010/result.json` and
`/tmp/wt-optimistic-ui-20261010/result.json`.

## Historical machine baseline (2026-10-06)

Machine: 12 cores / 32 GB Apple Silicon. The reported desktop freeze was a few
minutes before the initial report, approximately 09:13 local (16:13Z). Historical
samples at 16:13–16:14Z showed load 17–18, Vitest at 228–260% CPU, wt at
94–129%, native pg_cron near 100%, Brave near 100%, and WindowServer near 50%.
These were decaying `ps` averages, not instantaneous measurements or proof of
which process blocked the desktop.

Later samples had 42–52% CPU idle and stable 350 MB swap use, with no
swap/pageout/compression growth over eight seconds. This quieter baseline does
not disprove the earlier freeze. Small old Supabase Docker containers used
roughly 350 MB and less than 1% CPU; the hot database was native. Brave's
background thread still consumed a core. The wt main-thread sample was mostly
idle while a worker periodically became busy. Shared Codex daemon children can
fall outside wt's process ancestry; correlate cwd, session history, and command
times before assigning processes to other apps. RSS is not macOS physical
footprint: wt measured about 209 MB footprint despite roughly 900 MB RSS.

## Open issues

- **Live machine pressure (2026-10-13).** With builds finished, a second
  `top` sample showed 84.76% CPU busy: wt PID 32450 near 99% of one core,
  native pg_cron PIDs 4383/81482 near 80% each, and Brave PID 83591 near
  79%. The cron launchers belonged to local `r2-chat-attachments` and
  `instagram-live-qualification` stacks. Both used Unix-only PostgreSQL but
  cron targeted localhost; their latest 100 attempts were startup failures
  (one still connecting). Using the local `supabase_admin` role, reloading
  `cron.launch_active_jobs=off` reduced both launchers to 0% CPU while
  preserving each stack's 67 jobs and 23 active flags. No database or dev
  server was stopped. Local cron execution remains paused pending a verified
  startup/socket repair; this mitigation does not make scheduled jobs work.
  A later second sample was 51.60% idle, with wt still 108.3% and Brave
  79.4% of one core. No swapout growth. Evidence:
  `/tmp/wt-machine-top-after-build.txt`,
  `/tmp/wt-machine-top-cron-paused.txt`. These changing workloads are not
  a controlled before/after benchmark of wt.
  The new `new-frontend-enable` stack later developed the same failure:
  launcher PID 54920, Unix-only PostgreSQL, cron targeting localhost, and
  100 recent startup timeouts. The same reload-only pause preserved its
  67 jobs and 23 active flags; accumulated launcher CPU stayed at 36:38.45
  over 20 seconds and its database remained queryable. Final read-only checks
  confirmed the Instagram and new-frontend stacks still have cron execution
  paused, with 67 jobs and 23 active flags each. The r2 stack's former socket
  and PID are gone, so its current configuration could not be verified.
  Scheduled execution needs a socket/startup repair before resuming it.
- **Native Codex discovery CPU (2026-10-13).** The live v0.1.10 board
  PID 32450 consumed 95.8% of one core in the host's second `top` sample.
  A five-second native sample at 09:51 local put the main thread asleep for
  3,747/3,926 samples; blocking workers repeatedly scanned rollout headers,
  parsed main/manager ownership prefixes, and traversed historical directories
  for exact UUIDs. Evidence: `/tmp/wt-cpu-sample-20261013.txt` and
  `/tmp/wt-machine-top-20261013.txt`. Source repair caches both accepted and
  excluded parsed headers and ownership against size, nanosecond modification
  and change time, inode and device; entries are bounded and missing/replaced
  files invalidate them. Activity retains and revalidates exact UUID paths and
  refreshes only the changed Codex slots rather than every live harness slot.
  Optimized synthetic comparison (500 excluded 48 KiB headers, ten passes):
  uncached 217.51 ms versus warm cached 16.33 ms; initial cache population
  22.77 ms. This measures header scanning only, not whole-board CPU.
  Reproduce with `cargo test -p wt-harness rollout_discovery_cache_benchmark
  --release -- --ignored --nocapture`.
  These local optimizations did not fix sustained load: `22031c2` used 112.83%
  of one core over 240 seconds, and directory-index candidate `0cfbf8c` used
  140.64% over 180 seconds. Samples under `/tmp/wt-live-profile-22031` and
  `/tmp/wt-live-profile-0cfbf8c` showed a feedback loop: unknown tmux UUIDs made
  every historical session in the slot appear live; a cursor keyed only by
  slug alternated histories, reporting a change and requesting discovery on
  every pass. Discovery then woke activity again. A truly missing rollout
  without any readable histories merely retried at the ordinary backstop.
  Cursors now use slug, cwd and UUID; incomplete JSONL records do not report
  progress until a complete line is consumed. The regression fails against
  the original cursor implementation.
  An isolated optimized copy retaining the OLD permissive session selection
  but using the fixed cursors consumed 14.01% over 60 seconds and quit in
  0.004 seconds (`/tmp/wt-live-feedback-proof/result.json`). This validates the
  loop fix independently of the new strict identity filtering. These are
  UI-process CPU measurements, excluding subprocesses, on the real Cozee
  configuration with no sources disabled; other live work was uncontrolled.
  Seven existing daemon-backed Codex panes expose no provable current UUID.
  They now remain busy/unknown rather than claiming historical threads live.
  Empty-thread preallocation was tested and rejected: Codex 0.162.1 did not
  reliably materialize a resumable rollout before a turn, so no such startup
  change ships (`/tmp/wt-empty-thread-proof.txt`). The optimized `06cb306`
  candidate with strict identity filtering measured 11.08% over 60 seconds
  and quit in 0.003 seconds after refresh (`/tmp/wt-live-final-06cb306/result.json`).
  This lower measurement includes the unknown-session limitation above; the
  separate permissive-selection comparison proves the feedback fix itself.
  The optimized `d272490` candidate measured 8.55% over 60 seconds and quit in
  0.0034 seconds after refresh (`/tmp/wt-live-final-d272490/result.json`).
  Its full local gate and 15 native checks passed, as did cross-platform CI.
  Live MaxBook testing then blocked promotion: its legitimate 9 KiB PATH was
  duplicated into a 20 KiB tmux launch request. Session creation now streams
  tmux-native escaped arguments through stdin; a real 40 KiB environment test
  covers cold/warm servers and retained retries. Repeated managed PATH setup
  also preserves first occurrence order without growth. Candidate `848a223`
  passed all 899 tests and 15 native checks, plus three 20-second managed
  Codex attach/F12 cycles without a trust prompt or early exit. Nonforce remote
  cleanup exposed a missing controller-owned stop hook. `b9d8d2f` provisions
  explicitly declared managed files, repairs drift, and exports their worker
  root. It passed 903 local tests, all 15 native checks, cross-platform release
  verification, and live MaxBook attachment plus nonforce deletion. The helper
  matched its source; the QA checkout, branch, and session were removed while
  pre-existing process identities stayed unchanged. Published v0.1.13 now runs
  locally, on MaxBook, and in the restarted events daemon, whose snapshot is
  fresh. Candidate CPU/quit evidence is recorded in the baseline above.
- **FSEvents delivery during managed-remote QA (2026-10-12).** An isolated
  Codex rollout append watcher received no callbacks within either 2 or 15
  seconds, including a fixture under the home directory. Host sampling showed
  fseventsd at 100% CPU, load 9.41, wt at 234% of 683% CPU in use (decaying
  `ps` averages). Activity now checks metadata for its latest seven logs plus
  reports every second; Codex checks exact active rollout files every 500 ms.
  Both refresh only changed files and avoid reading unchanged contents. All
  15 focused activity tests pass, including native-watcher-disabled regressions
  under their original deadlines. Git freshness now checks metadata every
  second over immediate checkout children and recursive Git metadata only,
  bounded to 8,192 entries and 200 ms per scan. Unchanged samples trigger no
  Git commands; incomplete samples retain the 60-second backstop. Native-on
  and native-disabled dirty-to-clean regressions pass with the original
  five-second deadlines. Canonical watcher
  registration fixes mixed `/var` and `/private/var` aliases but does not prove
  reliable OS delivery. notify 8.2 teardown purges device events through the
  current id; interference from other streams remains unproven. No test
  deadlines were increased to hide missing notifications.
  A later macOS CI title-only edit test observed an unexpected Git rescan;
  its exact triggering event was not logged. Local tracing reproduced only
  the metadata lane and passed. Inspection found that delayed native file
  notifications could duplicate an invalidation already issued by that lane.
  Both now share a file-identity witness; unchanged known regular files skip
  the redundant invalidation, while directory/unknown paths and OS rescan
  flags remain conservative. A deterministic delayed-event regression covers
  later edits, replacements and deletion. Subsequent CI run 38439981305
  identified the title-test failure precisely: SQLite invalidated metadata
  only, then a queued origin fetch changed `FETCH_HEAD`, objects and remote
  refs. The earlier `r` scheduled that fetch behind its ten-second floor;
  `WT_GITHUB=off` does not disable origin maintenance. Holding a real second
  fetch until the title save reproduced the failure locally (29 to 34 Git
  scans; `/tmp/wt-ui-origin-repro-20261013`). The local UI fixture now sets
  `WT_FETCH_ORIGIN=off` and asserts zero fetch attempts before evaluating
  title isolation. Sections/feeds and TERM checks pass with zero title Git
  scans and the unchanged deadlines. This fixes fixture attribution, not a
  production watcher defect; broad OS-delivery reliability remains open.
- **Creation feedback must not create inventory (2026-10-13).** Earlier
  temporary creation rows could overlap the real row during remote refresh.
  Creation progress now lives in the header/footer; only host inventory can
  produce list rows. F12 queues entry for the exact resolved host/key and
  navigation releases that intent. Ordering regressions cover inventory before
  the action reply and matching slugs on different hosts. A candidate created,
  attached/re-attached three times, and safely removed a managed MaxBook QA
  worktree. Evidence: `/tmp/wt-f12-live-qa/managed-attach-{1,2,3}.raw`.
- **Native clean/delete confirmation waits on fresh remote evidence (2026-10-10).**
  User reports a long `working` state after `c` or `d`. Source inspection:
  `controller_actions` calls `lifecycle_ops::plan` before confirmation and
  again after delete confirmation. The planner fetches GitHub, then processes
  rows serially; local landing proof can also call `git ls-remote` once per
  repository (once per clone for Rift). `fleet_cleanup::prepare` waits for all
  configured hosts. GitHub's fetch budget is 100 seconds. TypeScript's initial
  removal guards use prepared board rows. No live deletion was run and no
  measured action timing establishes which request caused this report.
  Source fix now passes each host's existing GitHub snapshot into TUI planning
  and confirmation rechecks. Cached merged PRs require an exact HEAD match;
  local hazards and Git-only published-base checks remain. Installation and
  live key-to-confirmation timing are still owed. Serial planning and waiting
  for unavailable hosts can still add delay.
- **Local Rift creation can remain hidden until WT restarts (2026-10-09).**
  `tasks-take-2` and `secrets-check` TUI logs stop at `rift create --copy-all`
  at 20:50:13Z and 21:05:08Z. Their lock metadata records `init` at the same
  phase. Rift inventory hides rows under a live init lock. After the reported
  restart, neither creator PID (20158, 4199) nor a Rift process remained,
  and agents used both directories. This does not prove the filesystem
  watcher failed: no successful creation was logged before the restart.
  Capture the live creator, Rift child, pipe state, and lock liveness during
  the next occurrence. No processes or checkout files were changed.
- **Codex updater ownership and drain exits (2026-10-07).**
  `move-files-to-r2` exited at 09:56:51 local after `turn/steer failed: Server
  is draining; retry after reconnecting`. Native logs confirm repeated
  shutdown-signal restarts, but not the signal sender. Six same-user update
  loops coexist; three independently logged scheduled updates. Dotfiles c32b1d8
  fixed daemon argv recognition only. A singleton repair preserving the current
  daemon and native TUI reconnect handling remain owed; no updaters were
  stopped. Evidence: `/tmp/wt-daemon-followup-20261007/assessment.md`.
  Feature-list responsiveness does not prove that a draining server admits new
  turns.
- **Native Supabase cron startup remains broken.** CLI 2.119.0's native
  Postgres for set-your-status exposed only a Unix socket, while cron connected
  to localhost:5432. It accumulated 18,115 failed attempts and almost 15
  CPU-hours. Reloading `cron.launch_active_jobs=off` stopped the loop without
  changing the 67 jobs or 23 active flags; CPU time and attempt count stayed
  unchanged over 61 seconds. The worktree was later removed and its original
  processes were gone. Retained stack metadata is not evidence of an orphan.
  Future/restored stacks still need a functional startup fix (upstream Supabase
  CLI issue #6977), including database-test startup. Background-worker cron
  requires a controlled restart and functional verification. No Cozee source
  change was made.
- **Brave background CPU and severe desktop freezes remain unexplained.** PID
  57049 consumed about one core with its AppKit thread idle in a native sample.
  No browser state was changed. Reduced test concurrency and wt scan work
  remove demonstrated pressure; they do not prove elimination of every freeze
  or Codex's native feature-discovery timeout.

## Native runtime lessons

- **Cache excluded discovery records as well as matches.** A live session's
  activity used to rediscover every slot, each reparsing the recent session
  archive including unrelated tool/executor rollouts. Main/manager scans also
  reread up to 2 MiB to prove slot ownership. Match-only caches miss most of
  that cost. Validate negative entries against full file identity and retain
  exact activity paths; unchanged files should cause no content reads.
- **Refresh animation should stop with its finite drain.** The restored bar
  wave uses a 10 Hz timer only while refreshing/draining and reuses the prior
  screen to repaint its header. An isolated debug PTY fixture measured 0.25%
  of one core idle and 2.50% refreshing (including normal source work), 28
  distinct header frames in four seconds, 4.8 ms navigation, and zero bytes
  during settled idle or a help overlay. Header text positions remained fixed;
  resize and overlay resume passed. Evidence: `/tmp/wt-wave-native-59044/`.
  This is not an optimized production-board benchmark. Startup checks wait
  for bounded quiescence before their strict idle assertion; continuous frame
  emission still fails the fixed deadline.
- **Batch macOS watcher registration.** notify 8.2 restarts its FSEvents stream
  on each `watch` or `unwatch`. Per-path setup exceeded the native freshness
  regression's five-second registration limit under four test processes.
  `paths_mut` applies the changed inventory in one transaction; the same
  unchanged-limit regression then passed in 2.33 seconds total and the
  191-test app/TUI/runtime run passed. Skip the transaction for unchanged paths
  and keep blocking watcher setup and teardown off the UI path.
- **Optional observers must not own shared invalidation.** Dropping the
  automation edit-timestamp handle closed its request queue and previously
  ended the host filesystem watcher. Disable only that select branch when it
  closes. The Git/state watcher remains scoped to the host, and registration
  requests a follow-up Git scan to cover edits during initial watch setup.
- **Distinguish a same-version daemon feature mismatch from source behavior.**
  On Codex 0.161.0, the shared daemon reported
  `api_key_model_discovery=false` although the CLI default was true. The
  desktop rollout gate can set that runtime value without a config-file
  change. Dotfiles adc455c explicitly enables it in shared config, which takes
  precedence over host defaults. Readback matched all four native compatibility
  features without restarting PID 41633. A fresh TUI reached
  `/status: Local background server` in 1.877s. This fixed the mismatch, not
  the separate drain exits or historical timeouts.
- **Removal and reclamation are separate phases.** In the historical Rift
  implementation, `rift remove` moved a clone to trash and `rift gc` physically
  deleted it. The 16:46Z facebook-status removal coincided with a transient
  Rift/FSEventsd CPU spike, not proof that deletion caused a desktop freeze.
  GC had separate phase timing and requested nice 10 plus macOS background
  scheduling when available; the scheduling effect remained unverified. Do
  not delete live worktrees to reproduce load. An OS priority request does not
  guarantee zero latency.
- **Verify whether a process is a TUI orphan or a launchd service.** A historical
  sweep found 33 leaked headless wt instances, but a daemon parented to launchd
  can look similar. The native `wt perf` sampler excludes pids launchd claims
  under `com.wt.*` labels. Confirm with `launchctl list <label>` and
  `ps -o ppid,command`; propose cleanup rather than killing processes without
  authorization.
- **Keep measurement overhead and sampling semantics explicit.** `ps` `%CPU`
  is a decaying average; `top -l 2`'s second sample is closer to instantaneous.
  RSS is not physical footprint. A quiet idle sample does not rule out a prior
  spike. Record process identity, sampling window, and whether the measurement
  is cold, warm, or under active output.

### Native managed-removal follow-up, 2026-10-13

- On v0.1.14 (`25119ab`), removing an owned, clean local QA checkout
  removed its directory before the CLI returned. At 3m51s the native `rift gc`
  child reported 36.8% CPU through `ps`; the Node wrapper and waiting wt
  process reported 0%. This is a decaying CPU sample, not an interval average.
  The command later completed successfully without forced termination.
- The same release's owned MaxBook TUI test quit in 262ms after durable
  removal acceptance. The worker subsequently failed safely in the Cozee stop
  hook because a `--no-install` checkout lacked its pinned Supabase CLI.
  A quick terminal exit proves ownership transfer, not completed removal;
  inspect the durable job's terminal result separately.

## Historical TypeScript implementation evidence

These measurements describe the retired TypeScript/OpenTUI implementation;
they are not instructions for the native Rust runtime.

- A TypeScript-era `wt _home` process once burned 19 CPU-hours because Bun
  busy-spun on a bare pending promise. The fix was an inert interval
  (4e18459). This is historical and does not describe native worker behavior.
- OpenTUI Timeline rendering held a renderer-wide live request, with continuous
  roughly 60 fps full-tree walks and repaints, about 13% idle CPU per instance,
  and cost scaling with board size. A six-part series (f54910e through
  0094b27) reduced idle CPU to 0.1%. This behavior and its `WT_PERF` /
  `OTUI_*` probes belonged to the retired renderer.
- A TypeScript sealed-fixture sweep measured back-to-back render-thread stalls
  of 4,104 / 4,209 / 2,650 ms. The causes included O(N²) tracked-query
  combining, an unnecessarily broad refresh after removal, and synchronous
  `Bun.spawn` work. After fixes, the sweep's worst block was about 400 ms with
  input live; pressing `r` improved from 2,699 ms to 185 ms on a 22-row board.
  Do not transfer those implementation-specific causes or mitigations to Rust.
- A 2026-08-15 real-fleet TypeScript probe measured one Codex discovery scan at
  106.2 ms wall time while the main-thread 5 ms timer saw 0.8 ms maximum lag.
  Direct rollout lookup cost 25–107 ms per worktree and 355 ms over ten rows.
  Moving parsing/tailing to workers addressed that JavaScript main-thread cost.
- On 1,684 real Codex rollouts, a warm TypeScript-era scan read 1,480 excluded
  64 KiB prefixes (about 93 MB requested) in 270.78 ms. Caching complete
  exclusions against size, mtime, ctime, inode, and device reduced this to zero
  reads and 27.33 ms; cold scans were unchanged. Treat this as a historical
  measurement, not proof of current Rust scan cost.
- Deleting a 20k-file tree under macOS `fs.watch({recursive:true})` delivered
  233 events and 3.4 ms total callback time in the TypeScript implementation.
  The native watcher uses notify/FSEvents and has separate registration and
  lifecycle behavior.
