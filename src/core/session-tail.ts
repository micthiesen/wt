/**
 * Per-worktree tailer for the wt-managed interactive `claude` session
 * jsonl. Powers the activity-pane swap when an F12 session is live —
 * the same `ActionLine[]` shape the action runner produces, fed by
 * fs.watch on the file claude appends to.
 *
 * # Lifecycle
 *
 * `ensure(slug, wtPath)` is idempotent. The driver (`tui/app.tsx`)
 * calls it for every slug in the live tmux-session set on every
 * `useEffect`; new entries spin up a tailer, the rest no-op. `stop`
 * unwinds one tailer; `reconcile` runs ensure-and-stop together against
 * a fresh live set.
 *
 * # Seeding
 *
 * On first ensure, the tailer reads the last `SEED_TAIL_BYTES` of the
 * jsonl, parses entries forward, and surfaces the resulting lines as
 * the initial buffer. This means navigating to a freshly-attached row
 * after a wt restart shows real history immediately, not a blank pane.
 * `toolStarts` populates as we walk so tool_results that arrive after
 * the seed compute correct durations against pre-end-of-seed tool_uses.
 * Older entries beyond the seed window are not surfaced; tool_results
 * inside the window whose tool_use sits outside it render with `(—)`.
 *
 * # Live tail
 *
 * fs.watch fires per write; we coalesce with `READ_DEBOUNCE_MS` so a
 * burst of appends only triggers one stat+read pass. Reads are byte-
 * range from `lastByte` to the new size, parsed into entries, with
 * partial trailing fragments held in `pending` for the next read.
 *
 * # Pre-creation race
 *
 * `tmuxSessionsQuery` reports a session as live the moment tmux's
 * `new-session` returns, which can be milliseconds before claude's
 * first jsonl write. When the file isn't on disk yet, we fall back to
 * watching the parent project dir; the dir watcher promotes itself to
 * a file watcher (and seeds) the moment our uuid.jsonl appears.
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

import {
  type ActionLine,
  type ToolStartMap,
  MAX_BUFFERED_LINES,
  asObj,
  messageToLines,
} from "./claude-events.ts";
import { wtSessionUuid } from "./claude.ts";
import { createLogger } from "./logger.ts";

const log = createLogger("[session-tail]");

/** How many trailing bytes of the jsonl to seed from on first ensure. */
const SEED_TAIL_BYTES = 64 * 1024;
/** Coalesce window for fs.watch bursts. */
const READ_DEBOUNCE_MS = 80;

export type SessionRun = {
  slug: string;
  startedAt: number;
  lines: readonly ActionLine[];
};

type Listener = () => void;

type State = {
  path: string;
  projectDir: string;
  jsonlName: string;
  toolStarts: ToolStartMap;
  lastByte: number;
  pending: string;
  watcher: FSWatcher | null;
  dirWatcher: FSWatcher | null;
  debounce: Timer | null;
};

class SessionTailRegistry {
  private runs: ReadonlyMap<string, SessionRun> = new Map();
  private state = new Map<string, State>();
  private listeners = new Set<Listener>();

  /**
   * Idempotent. Spins up a tailer for `slug`'s wt-managed jsonl if not
   * already running; safe to call on every render-driven reconcile.
   */
  ensure(slug: string, wtPath: string): void {
    const uuid = wtSessionUuid(wtPath);
    const projectDir = join(
      homedir(),
      ".claude",
      "projects",
      wtPath.replaceAll("/", "-"),
    );
    const jsonlName = `${uuid}.jsonl`;
    const path = join(projectDir, jsonlName);
    const existing = this.state.get(slug);
    if (existing) {
      // Already tracking this slug — only restart if the resolved path
      // changed (e.g. a destroy+recreate cycle re-pointed the slug at a
      // different worktree path within the same TUI run).
      if (existing.path === path) return;
      this.stop(slug);
    }

    const st: State = {
      path,
      projectDir,
      jsonlName,
      toolStarts: new Map(),
      lastByte: 0,
      pending: "",
      watcher: null,
      dirWatcher: null,
      debounce: null,
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

  /**
   * Sync tracked tailers to a live `slug → wtPath` set. Spins up
   * tailers for new live slugs, stops tailers for slugs no longer live.
   * Designed to be called from a reactive effect with the current
   * tmux-session set.
   */
  reconcile(liveSlugs: ReadonlyMap<string, string>): void {
    for (const [slug, wtPath] of liveSlugs) this.ensure(slug, wtPath);
    for (const slug of [...this.state.keys()]) {
      if (!liveSlugs.has(slug)) this.stop(slug);
    }
  }

  /** Stop every tailer. Used on TUI shutdown. */
  stopAll(): void {
    for (const slug of [...this.state.keys()]) this.stop(slug);
  }

  get(slug: string): SessionRun | null {
    return this.runs.get(slug) ?? null;
  }

  getSnapshot = (): ReadonlyMap<string, SessionRun> => this.runs;

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

  private commit(mut: (m: Map<string, SessionRun>) => void): void {
    const next = new Map(this.runs);
    mut(next);
    this.runs = next;
    this.notify();
  }

  private update(slug: string, mut: (r: SessionRun) => SessionRun): void {
    const cur = this.runs.get(slug);
    if (!cur) return;
    this.commit((m) => m.set(slug, mut(cur)));
  }

  private watchForCreation(slug: string): void {
    const st = this.state.get(slug);
    if (!st) return;
    // Project dir may not exist if claude has never written for this
    // worktree before; create it so fs.watch has something to attach to.
    try {
      mkdirSync(st.projectDir, { recursive: true });
    } catch {
      // best-effort; if mkdir fails the watch will too and we log below
    }
    try {
      st.dirWatcher = watch(
        st.projectDir,
        { persistent: false },
        (_event, filename) => {
          // macOS reports `filename` as null on some atomic-write paths;
          // accept null (recheck) rather than dropping the event.
          if (filename != null && filename !== st.jsonlName) return;
          if (!existsSync(st.path)) return;
          closeSilent(st.dirWatcher);
          st.dirWatcher = null;
          this.seedAndWatch(slug);
        },
      );
    } catch (err) {
      log.warn("dir watch failed", {
        slug,
        projectDir: st.projectDir,
        err: errMsg(err),
      });
      return;
    }
    // Race close: claude can create the file between our pre-watch
    // existsSync (in ensure) and the watcher attaching. Without this
    // recheck the tailer would stay stuck on the dir watcher forever
    // even though the file is on disk and growing.
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
    const lines = body.split("\n");
    // Drop the first fragment if we didn't start at byte 0 — likely partial.
    // Tool_uses outside the seed window leave their tool_results in the
    // window without a matching start in `toolStarts`, so those results
    // render with `→ ok (—)`. Acceptable for a seed of bounded size.
    const startIdx = start === 0 ? 0 : 1;
    const accum: ActionLine[] = [];
    for (let i = startIdx; i < lines.length; i++) {
      const line = lines[i];
      if (!line) continue;
      const out = parseEntry(line, st.toolStarts);
      for (const l of out) accum.push(l);
    }
    st.lastByte = size;
    st.pending = "";
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
      // File shrank — claude rotated or external truncate. Resync.
      st.lastByte = size;
      st.pending = "";
      return;
    }
    const body = readBytes(st.path, st.lastByte, size - st.lastByte);
    st.lastByte = size;
    const combined = st.pending + body;
    const lines = combined.split("\n");
    st.pending = lines.pop() ?? "";
    const newLines: ActionLine[] = [];
    for (const line of lines) {
      if (!line) continue;
      const out = parseEntry(line, st.toolStarts);
      for (const l of out) newLines.push(l);
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

function parseEntry(raw: string, toolStarts: ToolStartMap): ActionLine[] {
  let evt: unknown;
  try {
    evt = JSON.parse(raw);
  } catch {
    return [];
  }
  const e = asObj(evt);
  if (!e) return [];
  const t = e.type;
  if (t !== "assistant" && t !== "user") return [];
  const ts = entryTs(e);
  return messageToLines({ role: t, message: e.message, ts, toolStarts });
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

function entryTs(e: Record<string, unknown>): number {
  const ts = e.timestamp;
  if (typeof ts === "string") {
    const parsed = Date.parse(ts);
    if (!Number.isNaN(parsed)) return parsed;
  }
  return Date.now();
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

export const sessionTailRegistry = new SessionTailRegistry();
