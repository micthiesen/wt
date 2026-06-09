/**
 * File tailer for action log files. Emits each newly-appended line
 * verbatim (no ANSI scrubbing — the registry decides per-kind whether
 * to parse as JSON or clean as raw shell output).
 *
 * Pattern-clones `shell-tail.ts`'s fs.watch + debounce + delta-read
 * loop with one simplification: no registry, no React shape — each
 * tail is its own little state machine, started and stopped explicitly
 * by the action registry. The tradeoff is a third copy of the seed +
 * dir-watch + readDelta scaffolding; factoring shared with
 * `shell-tail.ts` and `session-tail.ts` is a worthwhile cleanup that
 * we deliberately don't do here to keep this PR focused.
 *
 * # Why no MAX_BUFFERED_LINES cap
 *
 * The action registry already caps `lines` per run at MAX_BUFFERED_LINES;
 * the tail just hands lines off as they arrive, so capping here would
 * be redundant and would silently drop lines that the registry might
 * otherwise compact (e.g. claude's "result" event always being last).
 *
 * # Seeding
 *
 * On `start`, if the file already exists we seed from the last
 * `SEED_TAIL_BYTES` so a wt restart rejoins a long-running action
 * with recent context, not an empty pane. The first segment is
 * dropped when seeding from a non-zero offset (likely a partial
 * line); the trailing segment is held as `pending` for the next read.
 */
import {
  type FSWatcher,
  existsSync,
  readFileSync,
  statSync,
  watch,
} from "node:fs";
import { join } from "node:path";

import { createLogger } from "./logger.ts";
import { closeSilent, readFileSlice } from "./tail-util.ts";

const log = createLogger("[action-tail]");

/** How many trailing bytes of the log to seed from on first start. */
const SEED_TAIL_BYTES = 32 * 1024;
/** Coalesce window for fs.watch bursts. */
const READ_DEBOUNCE_MS = 80;
/**
 * Backstop polling cadence. Bun's `fs.watch` on macOS can silently miss
 * append events on long-lived logs, leaving the action viewer stuck on
 * the seeded "starting…" line until close() runs its final flush. One
 * shared interval re-uses the same delta-read pipeline; `readDelta`
 * short-circuits when `size === lastByte`, so the idle cost is one
 * stat per stream per tick.
 */
const POLL_INTERVAL_MS = 3_000;
/**
 * Backstop interval for `watchDoneSentinel`. FSEvents on a freshly-
 * created dir has a ~tens-of-ms warm-up window where events for files
 * written *after* `watch()` returns but *before* the kernel subscription
 * is hot can be silently dropped. Fast actions (e.g. a one-line `git
 * checkout`) finish inside that window, write `done.json`, and the
 * watcher never fires — leaving the run stuck at `status: running`
 * forever (until the next wt restart, when the boot reconciler picks it
 * up via `readDoneFile`). Polling existsSync every ~half-second covers
 * the gap cheaply.
 */
const DONE_POLL_INTERVAL_MS = 500;

const activeStreams = new Set<StreamState>();
let pollTimer: Timer | null = null;

function ensurePoller(): void {
  if (pollTimer) return;
  pollTimer = setInterval(() => {
    for (const st of activeStreams) {
      // Skip streams still in `watchForCreation` mode — the dirWatcher
      // promotes them to `seedAndWatch` when the file appears; a
      // pre-seed readDelta would race the seed's drop-first-partial
      // logic and duplicate content.
      if (st.watcher == null) continue;
      if (st.onLine) scheduleRead(st, st.onLine);
    }
  }, POLL_INTERVAL_MS);
}

function stopPollerIfIdle(): void {
  if (activeStreams.size > 0 || pollTimer == null) return;
  clearInterval(pollTimer);
  pollTimer = null;
}

export type LineSource = "stdout" | "stderr";

export type TailLine = {
  /** Raw text of one complete line (newline stripped). */
  text: string;
  /** Which file the line came from. */
  source: LineSource;
};

export type ActionTailHandle = {
  /** Stop watching and release file/dir watchers. Idempotent. */
  close: () => void;
};

type StreamState = {
  path: string;
  lastByte: number;
  pending: string;
  watcher: FSWatcher | null;
  dirWatcher: FSWatcher | null;
  debounce: Timer | null;
  source: LineSource;
  /** Stored on the state so `closeStream` can do a final flush-read
   *  before releasing watchers — without this we'd drop lines that
   *  arrived between the wrapper's last write and the done.json
   *  sentinel that triggers our close. */
  onLine: ((line: TailLine) => void) | null;
};

/**
 * Start tailing one run's `stream.log` and `stderr.log`. Lines are
 * emitted in arrival order with their source tag; the registry routes
 * them through the kind-specific parser.
 *
 * `seed` controls whether to read existing content on start:
 *  - true (default): read up to the last SEED_TAIL_BYTES so a restart
 *    rejoins a long-running action with context.
 *  - false: only emit lines that arrive *after* this call; used for
 *    fresh launches where the registry has already populated the
 *    initial line via the synthesized "starting" entry.
 */
export function startActionTail(opts: {
  runDir: string;
  onLine: (line: TailLine) => void;
  seed?: boolean;
}): ActionTailHandle {
  const { runDir, onLine, seed = true } = opts;
  const stdout = makeStream(join(runDir, "stream.log"), "stdout");
  const stderr = makeStream(join(runDir, "stderr.log"), "stderr");
  stdout.onLine = onLine;
  stderr.onLine = onLine;
  for (const st of [stdout, stderr]) {
    activeStreams.add(st);
    if (existsSync(st.path)) {
      seedAndWatch(st, onLine, seed);
    } else {
      watchForCreation(st, onLine, seed);
    }
  }
  ensurePoller();
  return {
    close() {
      for (const st of [stdout, stderr]) closeStream(st);
    },
  };
}

/**
 * Read every complete line from a run dir's stream + stderr log files
 * once, without setting up any watchers. Used by the boot reconciler
 * to re-populate the in-memory line buffer for terminal runs (the
 * files won't grow further, so a one-shot read is sufficient and
 * cheaper than holding fs.watch handles for completed runs).
 *
 * Reads up to the last `SEED_TAIL_BYTES` of each file, matching the
 * live tail's seed window so the rendered scrollback is consistent
 * across "boot rehydration" and "tail in flight" code paths.
 */
export function seedActionDir(opts: {
  runDir: string;
  onLine: (line: TailLine) => void;
}): void {
  const { runDir, onLine } = opts;
  for (const source of ["stdout", "stderr"] as const) {
    const filename = source === "stdout" ? "stream.log" : "stderr.log";
    const path = join(runDir, filename);
    if (!existsSync(path)) continue;
    const st = makeStream(path, source);
    try {
      readSeed(st, onLine);
    } catch (err) {
      log.warn("seedActionDir read failed", { path, err: errMsg(err) });
    }
  }
}

export type DoneSentinel = {
  /** Millisecond-precision timestamp the wrapper exited. Stamped from
   *  the on-disk `done.json` mtime — see the wrapper script's header
   *  comment for why we don't carry this in the file body. */
  endedAt: number;
  exitCode: number;
};

/**
 * Watch a run dir for the wrapper's `done.json` sentinel, parse it
 * when it appears, and fire `onDone` exactly once. Returns a handle
 * whose `close()` cancels the watch (cancelling after the sentinel
 * fired is a no-op).
 *
 * Uses the existing-file fast path when `done.json` is already on
 * disk at start (the boot reconciler depends on this for runs that
 * completed while wt was down).
 *
 * If the file appears but is unparseable, we log and bail: a malformed
 * sentinel is treated as "no signal" rather than synthesising a fake
 * exit. The boot reconciler's "session gone + status running" branch
 * will eventually mark the run as failed.
 */
export function watchDoneSentinel(opts: {
  runDir: string;
  onDone: (done: DoneSentinel) => void;
}): ActionTailHandle {
  const { runDir, onDone } = opts;
  const path = join(runDir, "done.json");
  let dirWatcher: FSWatcher | null = null;
  let pollTimer: Timer | null = null;
  let fired = false;

  const stopAll = (): void => {
    closeSilent(dirWatcher);
    dirWatcher = null;
    if (pollTimer) {
      clearInterval(pollTimer);
      pollTimer = null;
    }
  };

  const tryRead = (): boolean => {
    if (fired) return true;
    const sentinel = readDoneFile(path);
    if (!sentinel) return false;
    fired = true;
    onDone(sentinel);
    stopAll();
    return true;
  };

  if (tryRead()) {
    return { close: stopAll };
  }

  try {
    dirWatcher = watch(runDir, { persistent: false }, (_event, filename) => {
      if (filename != null && filename !== "done.json") return;
      tryRead();
    });
  } catch (err) {
    log.warn("done watcher failed", { runDir, err: errMsg(err) });
  }
  // Race window 1: file may have been written between the existsSync
  // check above and the `watch()` call landing.
  tryRead();
  // Race window 2: FSEvents on a freshly-created dir has a brief
  // warm-up gap where events for files written immediately after
  // `watch()` returns can be dropped. A fast shell action (a one-line
  // git checkout) finishes inside that gap. The poller covers it; it
  // also stops as soon as the file appears.
  if (!fired) {
    pollTimer = setInterval(() => {
      tryRead();
    }, DONE_POLL_INTERVAL_MS);
  }

  return { close: stopAll };
}

/**
 * Read + parse a `done.json` if present. `endedAt` comes from the
 * file's mtime so it reflects the actual exit moment (millisecond
 * precision, accurate for both live and boot-rehydration code paths).
 * Exported alongside `watchDoneSentinel` so the boot reconciler can
 * read a sentinel that was already on disk before wt started.
 */
export function readDoneFile(path: string): DoneSentinel | null {
  if (!existsSync(path)) return null;
  let mtime = 0;
  try {
    mtime = statSync(path).mtimeMs;
  } catch (err) {
    log.warn("done.json stat failed", { path, err: errMsg(err) });
    return null;
  }
  try {
    const raw = readFileSync(path, "utf8");
    const parsed = JSON.parse(raw) as Partial<DoneSentinel>;
    if (typeof parsed.exitCode !== "number") {
      log.warn("done.json missing exitCode", { path, parsed });
      return null;
    }
    return { endedAt: mtime, exitCode: parsed.exitCode };
  } catch (err) {
    log.warn("done.json parse failed", { path, err: errMsg(err) });
    return null;
  }
}

function makeStream(path: string, source: LineSource): StreamState {
  return {
    path,
    lastByte: 0,
    pending: "",
    watcher: null,
    dirWatcher: null,
    debounce: null,
    source,
    onLine: null,
  };
}

function seedAndWatch(
  st: StreamState,
  onLine: (line: TailLine) => void,
  seed: boolean,
): void {
  if (seed) {
    try {
      readSeed(st, onLine);
    } catch (err) {
      log.warn("seed read failed", { path: st.path, err: errMsg(err) });
    }
  } else {
    // Skip seeding by snapping `lastByte` to current size; subsequent
    // appends still trigger reads.
    try {
      st.lastByte = statSync(st.path).size;
    } catch {
      st.lastByte = 0;
    }
  }
  try {
    st.watcher = watch(st.path, { persistent: false }, () =>
      scheduleRead(st, onLine),
    );
  } catch (err) {
    log.warn("file watch failed", { path: st.path, err: errMsg(err) });
  }
}

function watchForCreation(
  st: StreamState,
  onLine: (line: TailLine) => void,
  seed: boolean,
): void {
  // The wrapper opens both log files via `>>`, so if the inner command
  // never writes anything to one stream the file may stay missing for
  // the duration of the run. Watch the parent dir for the file's
  // creation event, then hand off to the regular watch.
  const dir = join(st.path, "..");
  try {
    st.dirWatcher = watch(dir, { persistent: false }, (_event, filename) => {
      const target = st.path.split("/").pop();
      if (filename != null && filename !== target) return;
      if (!existsSync(st.path)) return;
      closeSilent(st.dirWatcher);
      st.dirWatcher = null;
      seedAndWatch(st, onLine, seed);
    });
  } catch (err) {
    log.warn("dir watch failed", { dir, err: errMsg(err) });
    return;
  }
  // Race window: file may have been created between the existsSync
  // check at startActionTail and the watch registration above.
  if (existsSync(st.path)) {
    closeSilent(st.dirWatcher);
    st.dirWatcher = null;
    seedAndWatch(st, onLine, seed);
  }
}

function readSeed(st: StreamState, onLine: (line: TailLine) => void): void {
  let size = 0;
  try {
    size = statSync(st.path).size;
  } catch {
    return;
  }
  if (size === 0) {
    st.lastByte = 0;
    return;
  }
  const start = Math.max(0, size - SEED_TAIL_BYTES);
  const body = readFileSlice(st.path, start, size - start);
  const segments = body.split("\n");
  // Drop the first fragment if we didn't start at byte 0 — likely partial.
  const startIdx = start === 0 ? 0 : 1;
  const trailing = segments[segments.length - 1] ?? "";
  const completed = segments.slice(startIdx, -1);
  for (const seg of completed) {
    if (seg.length === 0) continue;
    onLine({ text: seg, source: st.source });
  }
  st.lastByte = size;
  st.pending = trailing;
}

function scheduleRead(
  st: StreamState,
  onLine: (line: TailLine) => void,
): void {
  if (st.debounce) return;
  st.debounce = setTimeout(() => {
    st.debounce = null;
    try {
      readDelta(st, onLine);
    } catch (err) {
      log.warn("delta read failed", { path: st.path, err: errMsg(err) });
    }
  }, READ_DEBOUNCE_MS);
}

function readDelta(st: StreamState, onLine: (line: TailLine) => void): void {
  let size = 0;
  try {
    size = statSync(st.path).size;
  } catch {
    return;
  }
  if (size === st.lastByte) return;
  if (size < st.lastByte) {
    // File shrank — external truncate. Resync.
    st.lastByte = size;
    st.pending = "";
    return;
  }
  const body = readFileSlice(st.path, st.lastByte, size - st.lastByte);
  st.lastByte = size;
  const combined = st.pending + body;
  const segments = combined.split("\n");
  st.pending = segments.pop() ?? "";
  for (const seg of segments) {
    if (seg.length === 0) continue;
    onLine({ text: seg, source: st.source });
  }
}

function closeStream(st: StreamState): void {
  // Stop watchers first so a write during the final readDelta below
  // doesn't schedule a new debounce (we want this to be the last read).
  closeSilent(st.watcher);
  closeSilent(st.dirWatcher);
  st.watcher = null;
  st.dirWatcher = null;
  if (st.debounce) clearTimeout(st.debounce);
  st.debounce = null;
  // Final flush: pull any bytes the wrapper wrote between the last
  // debounced read and the done.json sentinel that triggered close.
  // Without this, line-of-output races where stdout flushes microseconds
  // before the EXIT trap can drop the last line in live mode (the
  // file is still on disk, so a subsequent boot would see it; but
  // the live pane would mysteriously miss the last line of output).
  if (st.onLine && existsSync(st.path)) {
    try {
      readDelta(st, st.onLine);
    } catch {
      // best-effort
    }
  }
  st.onLine = null;
  activeStreams.delete(st);
  stopPollerIfIdle();
}

function errMsg(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}
