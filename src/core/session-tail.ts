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
  compactMeta,
  formatTokens,
  messageToLines,
  splitMessage,
} from "./claude-events.ts";
import { wtSessionUuid } from "./claude.ts";
import { createLogger } from "./logger.ts";

const log = createLogger("[session-tail]");

/**
 * Composite identifier for a session tail. `null` name = primary
 * (key collapses to the bare slug, matching the tmux session name);
 * a string name yields `<slug>~<name>`. Stable across the codebase
 * so anything that touches a session-tail map agrees on the key.
 */
export function tailKey(slug: string, name: string | null): string {
  return name === null ? slug : `${slug}~${name}`;
}

/** How many trailing bytes of the jsonl to seed from on first ensure. */
const SEED_TAIL_BYTES = 64 * 1024;
/** Coalesce window for fs.watch bursts. */
const READ_DEBOUNCE_MS = 80;

export type SessionRun = {
  slug: string;
  /** `null` = primary, otherwise the user-typed name. */
  name: string | null;
  startedAt: number;
  lines: readonly ActionLine[];
};

type Listener = () => void;

type State = {
  slug: string;
  name: string | null;
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

/**
 * Description of one live claude session for reconciliation. The
 * registry needs (slug, name, wtPath) per session: slug+name compose
 * the tail key; wtPath resolves the jsonl path via `wtSessionUuid`.
 */
export type LiveSessionDesc = {
  slug: string;
  /** `null` = primary. */
  name: string | null;
  wtPath: string;
};

class SessionTailRegistry {
  // Keys here are tail keys (tmux session names) — `<slug>` for
  // primary, `<slug>~<name>` for named. Multiple sessions per slug
  // coexist as separate entries.
  private runs: ReadonlyMap<string, SessionRun> = new Map();
  private state = new Map<string, State>();
  private listeners = new Set<Listener>();

  /**
   * Idempotent. Spins up a tailer for the (slug, name) session's
   * wt-managed jsonl if not already running; safe to call on every
   * render-driven reconcile.
   */
  ensure(slug: string, wtPath: string, name: string | null = null): void {
    const key = tailKey(slug, name);
    const uuid = wtSessionUuid(wtPath, name ?? undefined);
    const projectDir = join(
      homedir(),
      ".claude",
      "projects",
      wtPath.replaceAll("/", "-"),
    );
    const jsonlName = `${uuid}.jsonl`;
    const path = join(projectDir, jsonlName);
    const existing = this.state.get(key);
    if (existing) {
      // Already tracking — only restart if the resolved path changed
      // (e.g. a destroy+recreate cycle re-pointed the slug at a
      // different worktree path within the same TUI run).
      if (existing.path === path) return;
      this.stop(slug, name);
    }

    const st: State = {
      slug,
      name,
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
    this.state.set(key, st);

    const startedAt = Date.now();
    this.commit((m) => m.set(key, { slug, name, startedAt, lines: [] }));

    if (existsSync(path)) {
      this.seedAndWatch(key);
    } else {
      this.watchForCreation(key);
    }
  }

  stop(slug: string, name: string | null = null): void {
    const key = tailKey(slug, name);
    const st = this.state.get(key);
    if (!st) return;
    closeSilent(st.watcher);
    closeSilent(st.dirWatcher);
    if (st.debounce) clearTimeout(st.debounce);
    this.state.delete(key);
    this.commit((m) => {
      m.delete(key);
    });
  }

  /**
   * Sync tracked tailers to a live session set. Spins up tailers for
   * new live sessions, stops tailers for sessions no longer live.
   * Designed to be called from a reactive effect with the current
   * tmux-session list.
   */
  reconcile(live: readonly LiveSessionDesc[]): void {
    const liveKeys = new Set<string>();
    for (const desc of live) {
      liveKeys.add(tailKey(desc.slug, desc.name));
      this.ensure(desc.slug, desc.wtPath, desc.name);
    }
    for (const key of [...this.state.keys()]) {
      if (liveKeys.has(key)) continue;
      const run = this.runs.get(key);
      if (run) this.stop(run.slug, run.name);
      else this.state.delete(key);
    }
  }

  /** Stop every tailer. Used on TUI shutdown. */
  stopAll(): void {
    for (const key of [...this.state.keys()]) {
      const run = this.runs.get(key);
      if (run) this.stop(run.slug, run.name);
      else this.state.delete(key);
    }
  }

  /**
   * Lookup by (slug, name). Convenience overload `get(slug)` returns
   * primary — same as the prior single-session API.
   */
  get(slug: string, name: string | null = null): SessionRun | null {
    return this.runs.get(tailKey(slug, name)) ?? null;
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

  private update(key: string, mut: (r: SessionRun) => SessionRun): void {
    const cur = this.runs.get(key);
    if (!cur) return;
    this.commit((m) => m.set(key, mut(cur)));
  }

  private watchForCreation(key: string): void {
    const st = this.state.get(key);
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
          this.seedAndWatch(key);
        },
      );
    } catch (err) {
      log.warn("dir watch failed", {
        slug: st.slug,
        name: st.name,
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
      this.seedAndWatch(key);
    }
  }

  private seedAndWatch(key: string): void {
    const st = this.state.get(key);
    if (!st) return;
    try {
      this.readSeed(key);
    } catch (err) {
      log.warn("seed read failed", { slug: st.slug, name: st.name, err: errMsg(err) });
    }
    try {
      st.watcher = watch(st.path, { persistent: false }, () =>
        this.scheduleRead(key),
      );
    } catch (err) {
      log.warn("file watch failed", { slug: st.slug, name: st.name, err: errMsg(err) });
    }
  }

  private readSeed(key: string): void {
    const st = this.state.get(key);
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
    this.update(key, (r) => ({ ...r, lines: trimmed }));
  }

  private scheduleRead(key: string): void {
    const st = this.state.get(key);
    if (!st) return;
    if (st.debounce) return;
    st.debounce = setTimeout(() => {
      st.debounce = null;
      try {
        this.readDelta(key);
      } catch (err) {
        log.warn("delta read failed", { slug: st.slug, name: st.name, err: errMsg(err) });
      }
    }, READ_DEBOUNCE_MS);
  }

  private readDelta(key: string): void {
    const st = this.state.get(key);
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
    this.update(key, (r) => {
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
  const ts = entryTs(e);
  if (t === "assistant" || t === "user") {
    // The auto-injected post-compaction summary blob ("This session is
    // being continued from a previous conversation…") arrives as a
    // user envelope flagged with `isCompactSummary`. Skip it — the
    // `compact_boundary` system event below carries the high-signal
    // marker (token deltas, trigger), and rendering the full summary
    // would dump several hundred lines of internal detail into the pane.
    if (t === "user" && e.isCompactSummary === true) return [];
    return messageToLines({ role: t, message: e.message, ts, toolStarts });
  }
  // system.compact_boundary — fired when the conversation is compacted
  // (manual `/compact` or auto when the context window fills). Surface
  // as a single dim marker line so the user can see where compactions
  // landed in the timeline. Token counts come from `compactMetadata`;
  // we annotate the trigger only when it's `auto` since manual is the
  // common case (the user just typed /compact).
  if (t === "system" && e.subtype === "compact_boundary") {
    const meta = asObj(e.compactMetadata);
    const pre =
      meta && typeof meta.preTokens === "number" ? meta.preTokens : null;
    const post =
      meta && typeof meta.postTokens === "number" ? meta.postTokens : null;
    const trigger =
      meta && typeof meta.trigger === "string" ? meta.trigger : null;
    const tokenPart =
      pre != null && post != null
        ? ` (${formatTokens(pre)} → ${formatTokens(post)})`
        : "";
    const triggerPart = trigger && trigger !== "manual" ? ` ${trigger}` : "";
    return [
      { ts, kind: "info", text: `↘ compacted${triggerPart}${tokenPart}` },
    ];
  }
  // system.away_summary — claude's auto-generated context-recap when
  // the conversation is auto-compacted. High-signal: the user can
  // glance at the pane and see "this is what the previous turns were
  // about" without re-reading the whole tail. Multi-line: same
  // newline-split + per-line cap as assistant text, with the leading
  // `─` only on the first row so the block reads as one summary
  // group rather than a series of dash bullets.
  if (t === "system" && e.subtype === "away_summary") {
    const content = typeof e.content === "string" ? e.content : "";
    const { pieces, truncated } = splitMessage(content);
    if (pieces.length === 0) return [];
    const lines: ActionLine[] = pieces.map((piece, i) => ({
      ts,
      kind: "info",
      text: `${i === 0 ? "─ " : "  "}${piece}`,
    }));
    if (truncated > 0) {
      lines.push({
        ts,
        kind: "info",
        text: `  …${truncated} more line${truncated === 1 ? "" : "s"} truncated`,
      });
    }
    return lines;
  }
  // attachment.queued_command — when the user types-ahead while
  // claude is still processing, the prompt sits queued. Surface the
  // user-typed ones (`commandMode === "prompt"`) so the pane shows
  // intent that would otherwise be invisible. `task-notification`
  // and other system-injected attachments stay dropped.
  if (t === "attachment") {
    const att = asObj(e.attachment);
    if (
      att &&
      att.type === "queued_command" &&
      att.commandMode === "prompt" &&
      typeof att.prompt === "string"
    ) {
      const compacted = compactMeta(att.prompt);
      if (compacted) {
        return [{ ts, kind: "info", text: `⏎ queued: ${compacted}` }];
      }
    }
    return [];
  }
  return [];
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
