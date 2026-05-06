/**
 * Per-worktree tailer for the wt-managed F10 shell tmux session. Symmetric
 * to `core/session-tail.ts` (which tails F12 claude's jsonl), but the
 * source is a plain pipe-pane log file rather than structured stream-json.
 *
 * # Capture pipeline
 *
 * `attachOrCreate(kind: "shell")` chains a `tmux pipe-pane -o` after
 * `new-session -A -d` so every byte the shell writes to its pane gets
 * appended to `shellLogPath(slug)`. From there it's the same fs.watch
 * + debounce + delta-read pattern as session-tail.
 *
 * # ANSI stripping & carriage returns
 *
 * Pipe-pane captures the raw pty stream — full of colors, cursor moves,
 * OSC title sets, alt-screen toggles. We strip the lot with the same
 * regex that `core/tmux.ts`'s stderr scrubber uses, then collapse `\r`
 * by keeping only the segment after the last CR per line (so progress
 * bars like `npm install`'s render as their final state instead of as
 * one giant smear of overwrites).
 *
 * # Line discipline
 *
 * Pipe-pane has no notion of "line". We split the captured byte stream
 * on `\n`, hold the trailing fragment in `pending` for the next read,
 * and ring-buffer to `MAX_BUFFERED_LINES` so a runaway prompt loop
 * can't OOM the TUI.
 */
import {
  type FSWatcher,
  closeSync,
  existsSync,
  mkdirSync,
  openSync,
  readSync,
  statSync,
  watch,
} from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

import { MAX_BUFFERED_LINES } from "./claude-events.ts";
import { createLogger } from "./logger.ts";

const log = createLogger("[shell-tail]");

/** How many trailing bytes of the pipe-pane log to seed from on first ensure. */
const SEED_TAIL_BYTES = 32 * 1024;
/** Coalesce window for fs.watch bursts. */
const READ_DEBOUNCE_MS = 80;

// CSI + OSC + bare ESC — same regex as core/tmux.ts's stderr scrubber.
// eslint-disable-next-line no-control-regex
const ANSI_RE = /\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]/g;
// Other C0 control characters we want to drop (BEL, backspace, etc.) —
// keep \t (renders as space-runs) and let \r flow through to the
// progress-collapse step before final cleanup.
// eslint-disable-next-line no-control-regex
const CTRL_RE = /[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]/g;

/** Stable per-slug log path under wt's cache dir. */
export function shellLogPath(slug: string): string {
  const dir = join(homedir(), ".cache", "wt", "shell-logs");
  mkdirSync(dir, { recursive: true });
  return join(dir, `${slug}.log`);
}

export type ShellLine = {
  /** Stable id for React keys; monotonic across the session. */
  id: number;
  text: string;
  ts: number;
};

export type ShellRun = {
  slug: string;
  startedAt: number;
  lines: readonly ShellLine[];
};

type Listener = () => void;

type State = {
  path: string;
  lastByte: number;
  pending: string;
  watcher: FSWatcher | null;
  dirWatcher: FSWatcher | null;
  debounce: Timer | null;
  /** Monotonic line id counter — restarts at 0 per slug. */
  nextId: number;
};

class ShellTailRegistry {
  private runs: ReadonlyMap<string, ShellRun> = new Map();
  private state = new Map<string, State>();
  private listeners = new Set<Listener>();

  /**
   * Idempotent. Spins up a tailer for `slug`'s pipe-pane log if not
   * already running. Safe to call on every render-driven reconcile.
   */
  ensure(slug: string): void {
    const path = shellLogPath(slug);
    const existing = this.state.get(slug);
    if (existing) {
      if (existing.path === path) return;
      this.stop(slug);
    }

    const st: State = {
      path,
      lastByte: 0,
      pending: "",
      watcher: null,
      dirWatcher: null,
      debounce: null,
      nextId: 0,
    };
    this.state.set(slug, st);

    const startedAt = Date.now();
    this.commit((m) => m.set(slug, { slug, startedAt, lines: [] }));

    if (existsSync(path)) {
      this.seedAndWatch(slug);
    } else {
      this.watchForCreation(slug);
    }
  }

  stop(slug: string): void {
    const st = this.state.get(slug);
    if (!st) return;
    closeSilent(st.watcher);
    closeSilent(st.dirWatcher);
    if (st.debounce) clearTimeout(st.debounce);
    this.state.delete(slug);
    this.commit((m) => {
      m.delete(slug);
    });
  }

  reconcile(liveSlugs: ReadonlySet<string>): void {
    for (const slug of liveSlugs) this.ensure(slug);
    for (const slug of [...this.state.keys()]) {
      if (!liveSlugs.has(slug)) this.stop(slug);
    }
  }

  stopAll(): void {
    for (const slug of [...this.state.keys()]) this.stop(slug);
  }

  get(slug: string): ShellRun | null {
    return this.runs.get(slug) ?? null;
  }

  getSnapshot = (): ReadonlyMap<string, ShellRun> => this.runs;

  subscribe = (fn: Listener): (() => void) => {
    this.listeners.add(fn);
    return () => {
      this.listeners.delete(fn);
    };
  };

  // ---------- internals ----------

  private notify(): void {
    for (const l of this.listeners) {
      try {
        l();
      } catch {
        // sink errors must not break dispatch
      }
    }
  }

  private commit(mut: (m: Map<string, ShellRun>) => void): void {
    const next = new Map(this.runs);
    mut(next);
    this.runs = next;
    this.notify();
  }

  private update(slug: string, mut: (r: ShellRun) => ShellRun): void {
    const cur = this.runs.get(slug);
    if (!cur) return;
    this.commit((m) => m.set(slug, mut(cur)));
  }

  private watchForCreation(slug: string): void {
    const st = this.state.get(slug);
    if (!st) return;
    const dir = join(st.path, "..");
    try {
      st.dirWatcher = watch(dir, { persistent: false }, (_event, filename) => {
        const target = st.path.split("/").pop();
        if (filename != null && filename !== target) return;
        if (!existsSync(st.path)) return;
        closeSilent(st.dirWatcher);
        st.dirWatcher = null;
        this.seedAndWatch(slug);
      });
    } catch (err) {
      log.warn("dir watch failed", { slug, dir, err: errMsg(err) });
      return;
    }
    if (existsSync(st.path)) {
      closeSilent(st.dirWatcher);
      st.dirWatcher = null;
      this.seedAndWatch(slug);
    }
  }

  private seedAndWatch(slug: string): void {
    const st = this.state.get(slug);
    if (!st) return;
    try {
      this.readSeed(slug);
    } catch (err) {
      log.warn("seed read failed", { slug, err: errMsg(err) });
    }
    try {
      st.watcher = watch(st.path, { persistent: false }, () =>
        this.scheduleRead(slug),
      );
    } catch (err) {
      log.warn("file watch failed", { slug, err: errMsg(err) });
    }
  }

  private readSeed(slug: string): void {
    const st = this.state.get(slug);
    if (!st) return;
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
    const body = readBytes(st.path, start, size - start);
    const segments = body.split("\n");
    // Drop the first fragment if we didn't start at byte 0 — likely partial.
    const startIdx = start === 0 ? 0 : 1;
    // Last segment is the in-progress line; keep it as `pending`.
    const trailing = segments[segments.length - 1] ?? "";
    const completed = segments.slice(startIdx, -1);
    const ts = Date.now();
    const accum: ShellLine[] = [];
    for (const seg of completed) {
      const cleaned = clean(seg);
      if (cleaned === null) continue;
      accum.push({ id: st.nextId++, text: cleaned, ts });
    }
    st.lastByte = size;
    st.pending = trailing;
    const trimmed =
      accum.length > MAX_BUFFERED_LINES
        ? accum.slice(-MAX_BUFFERED_LINES)
        : accum;
    this.update(slug, (r) => ({ ...r, lines: trimmed }));
  }

  private scheduleRead(slug: string): void {
    const st = this.state.get(slug);
    if (!st) return;
    if (st.debounce) return;
    st.debounce = setTimeout(() => {
      st.debounce = null;
      try {
        this.readDelta(slug);
      } catch (err) {
        log.warn("delta read failed", { slug, err: errMsg(err) });
      }
    }, READ_DEBOUNCE_MS);
  }

  private readDelta(slug: string): void {
    const st = this.state.get(slug);
    if (!st) return;
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
    const body = readBytes(st.path, st.lastByte, size - st.lastByte);
    st.lastByte = size;
    const combined = st.pending + body;
    const segments = combined.split("\n");
    st.pending = segments.pop() ?? "";
    const ts = Date.now();
    const newLines: ShellLine[] = [];
    for (const seg of segments) {
      const cleaned = clean(seg);
      if (cleaned === null) continue;
      newLines.push({ id: st.nextId++, text: cleaned, ts });
    }
    if (newLines.length === 0) return;
    this.update(slug, (r) => {
      const merged = [...r.lines, ...newLines];
      const lines =
        merged.length > MAX_BUFFERED_LINES
          ? merged.slice(-MAX_BUFFERED_LINES)
          : merged;
      return { ...r, lines };
    });
  }
}

/**
 * Strip ANSI + collapse carriage returns + drop control chars. Returns
 * `null` for lines that are empty after cleanup so the buffer doesn't
 * fill with blanks from cursor-only escape sequences.
 */
function clean(raw: string): string | null {
  let s = raw.replace(ANSI_RE, "");
  // CR collapse: keep only what was on the line after the last \r.
  // Progress bars (`npm install`, `pnpm`, `yarn`) render via repeated
  // \r-overwrites of the same line; pipe-pane captures every redraw,
  // so this drops the dozens of intermediate states and keeps the last.
  const cr = s.lastIndexOf("\r");
  if (cr !== -1) s = s.slice(cr + 1);
  s = s.replace(CTRL_RE, "");
  s = s.trimEnd();
  return s.length === 0 ? null : s;
}

function readBytes(path: string, start: number, len: number): string {
  const fd = openSync(path, "r");
  try {
    const buf = Buffer.alloc(len);
    readSync(fd, buf, 0, len, start);
    return buf.toString("utf8");
  } finally {
    closeSync(fd);
  }
}

function closeSilent(w: FSWatcher | null): void {
  if (!w) return;
  try {
    w.close();
  } catch {
    // best-effort
  }
}

function errMsg(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

export const shellTailRegistry = new ShellTailRegistry();
