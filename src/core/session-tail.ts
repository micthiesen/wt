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
import { join } from "node:path";

import {
  type ActionLine,
  type MessageEmit,
  type ToolStartMap,
  AWAY_RECAP_HINT_RE,
  MAX_BUFFERED_LINES,
  asObj,
  compactMeta,
  formatTokens,
  messageToLines,
  splitMessage,
} from "./claude-events.ts";
import { projectDir as claudeProjectDir, wtSessionUuid } from "./claude.ts";
import { createLogger } from "./logger.ts";
import {
  detectRefreshTriggers,
  type RefreshTarget,
} from "./session-triggers.ts";
import { claudeSessionName } from "./tmux.ts";

const log = createLogger("[session-tail]");

// ---------------------------------------------------------------------------
// Refresh triggers
//
// While the live tail is reading the jsonl anyway, it scans each new
// entry for Bash tool calls (`gh pr create`, `git push`, …) that change
// GitHub-side state and asks the runtime to invalidate the matching
// query — see `session-triggers.ts`. Detection is per-line; delivery is
// debounced per target so a burst of git/gh calls collapses to one
// refresh, and so the triggering command has finished by the time the
// refetch fires (we match on tool_use, not tool_result).
// ---------------------------------------------------------------------------

/** Debounce window for refresh triggers. See block comment above. */
const TRIGGER_DEBOUNCE_MS = 3_000;

/** Tighter window for the per-slug "this jsonl moved" sink. The 80ms
 *  read-debounce already coalesces FSEvents bursts at the file level;
 *  this just collapses a turn's worth of appends into one invalidation
 *  pass so `wtClaudeQuery` (ages, queue counts) snaps on turn end
 *  instead of drifting up to its 5s poll. */
const SLUG_CHANGE_DEBOUNCE_MS = 500;

let triggerSink: ((target: RefreshTarget) => void) | null = null;
const triggerTimers = new Map<RefreshTarget, ReturnType<typeof setTimeout>>();
let slugChangeSink: ((slug: string) => void) | null = null;
const slugChangeTimers = new Map<string, ReturnType<typeof setTimeout>>();

/**
 * Register the refresh-trigger sink. The TUI runtime wires this to the
 * QueryClient so a `gh pr create` in a live session invalidates
 * `["github"]` immediately instead of waiting out its slow staleTime.
 * CLI runs leave it unset — triggers are a silent no-op there. Pass
 * `null` on shutdown; pending debounce timers are cleared.
 */
export function setSessionTriggerSink(
  fn: ((target: RefreshTarget) => void) | null,
): void {
  triggerSink = fn;
  if (!fn) {
    for (const t of triggerTimers.values()) clearTimeout(t);
    triggerTimers.clear();
  }
}

/**
 * Register the per-slug claude-jsonl-moved sink. Fires (debounced) when
 * any live session jsonl for that slug grows — the runtime invalidates
 * `qk.wt(slug).claude()` so the row's last-activity age and queue count
 * snap on turn end. Scoped to one slug; the broader `["github"]` &c
 * still come through `setSessionTriggerSink` only when the parser spots
 * a `gh`/`git` Bash call.
 */
export function setSessionSlugChangeSink(
  fn: ((slug: string) => void) | null,
): void {
  slugChangeSink = fn;
  if (!fn) {
    for (const t of slugChangeTimers.values()) clearTimeout(t);
    slugChangeTimers.clear();
  }
}

/**
 * Debounced per-target dispatch. Resets the timer on every hit, so a
 * burst collapses to a single refresh `TRIGGER_DEBOUNCE_MS` after the
 * last trigger.
 */
function scheduleTrigger(target: RefreshTarget): void {
  const existing = triggerTimers.get(target);
  if (existing) clearTimeout(existing);
  triggerTimers.set(
    target,
    setTimeout(() => {
      triggerTimers.delete(target);
      log.debug("refresh trigger fired", { target });
      triggerSink?.(target);
    }, TRIGGER_DEBOUNCE_MS),
  );
}

function scheduleSlugChange(slug: string): void {
  if (!slugChangeSink) return;
  const existing = slugChangeTimers.get(slug);
  if (existing) clearTimeout(existing);
  slugChangeTimers.set(
    slug,
    setTimeout(() => {
      slugChangeTimers.delete(slug);
      slugChangeSink?.(slug);
    }, SLUG_CHANGE_DEBOUNCE_MS),
  );
}

/**
 * Composite identifier for a session tail. By construction the same
 * string as the underlying tmux session name (`<slug>` for primary,
 * `<slug>~<name>` for named) — sharing one composition rule means the
 * tail-registry map and tmux's session list always agree on keys.
 */
export function tailKey(slug: string, name: string | null): string {
  return claudeSessionName(slug, name);
}

/** How many trailing bytes of the jsonl to seed from on first ensure. */
const SEED_TAIL_BYTES = 64 * 1024;
/** Coalesce window for fs.watch bursts. */
const READ_DEBOUNCE_MS = 80;
/**
 * Backstop polling cadence. Bun's `fs.watch` on macOS can silently miss
 * append events on long-lived jsonls (e.g. the main-clone slot's bottom-
 * bar tail going stale even though claude is still writing). One shared
 * interval iterates every tracked tail and re-uses the same delta-read
 * pipeline; `readDelta` short-circuits when `size === lastByte`, so the
 * idle cost is one stat per tailer per tick.
 */
const POLL_INTERVAL_MS = 3_000;

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
  /**
   * Monotonic per-tail line id. Tool calls stash their assigned id on
   * the `ToolStartEntry` so the later `tool_result` can patch the
   * same buffer line in place (collapsing the `⚒ → ✓` two-line pair
   * into one line that flips green/red). Survives seed→live handoff;
   * the seed pass and the live pass share the same counter so ids
   * stay unique across the boundary.
   */
  nextLineId: number;
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
  private poller: Timer | null = null;

  /**
   * Idempotent. Spins up a tailer for the (slug, name) session's
   * wt-managed jsonl if not already running; safe to call on every
   * render-driven reconcile.
   */
  ensure(slug: string, wtPath: string, name: string | null = null): void {
    const key = tailKey(slug, name);
    const uuid = wtSessionUuid(wtPath, name);
    const projectDir = claudeProjectDir(wtPath);
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
      nextLineId: 1,
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
    this.ensurePoller();
  }

  stop(slug: string, name: string | null = null): void {
    this.stopByKey(tailKey(slug, name));
  }

  /**
   * Close watchers/timers + drop both maps for `key`. Centralizing
   * the cleanup ensures `reconcile` / `stopAll` paths can't
   * accidentally orphan an FSWatcher or debounce Timer if `runs` and
   * `state` ever fall out of sync.
   */
  private stopByKey(key: string): void {
    const st = this.state.get(key);
    if (!st) return;
    closeSilent(st.watcher);
    closeSilent(st.dirWatcher);
    if (st.debounce) clearTimeout(st.debounce);
    this.state.delete(key);
    this.commit((m) => {
      m.delete(key);
    });
    if (this.state.size === 0) this.stopPoller();
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
      if (!liveKeys.has(key)) this.stopByKey(key);
    }
  }

  /** Stop every tailer. Used on TUI shutdown. */
  stopAll(): void {
    for (const key of [...this.state.keys()]) this.stopByKey(key);
    this.stopPoller();
    // Drop any pending debounce timers — no tailer is left to have
    // produced them, and a late fire would invalidate queries on a
    // torn-down client. Both maps, symmetrically: the runtime also nulls
    // the sinks on shutdown, but stopAll shouldn't depend on that.
    for (const t of triggerTimers.values()) clearTimeout(t);
    triggerTimers.clear();
    for (const t of slugChangeTimers.values()) clearTimeout(t);
    slugChangeTimers.clear();
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

  private ensurePoller(): void {
    if (this.poller) return;
    this.poller = setInterval(() => {
      for (const [key, st] of this.state) {
        // Skip tails still in `watchForCreation` mode — the dirWatcher
        // promotes them to `seedAndWatch` when the file appears, and a
        // pre-seed `readDelta` would race the seed's drop-first-partial
        // logic and duplicate content.
        if (st.watcher == null) continue;
        this.scheduleRead(key);
      }
    }, POLL_INTERVAL_MS);
  }

  private stopPoller(): void {
    if (!this.poller) return;
    clearInterval(this.poller);
    this.poller = null;
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
    // render as standalone `→ ok (—)` orphan lines via the orphan path
    // in messageToLines. Acceptable for a seed of bounded size.
    const startIdx = start === 0 ? 0 : 1;
    let accum: ActionLine[] = [];
    const nextId = () => st.nextLineId++;
    for (let i = startIdx; i < lines.length; i++) {
      const line = lines[i];
      if (!line) continue;
      const out = parseEntry(line, st.toolStarts, nextId);
      accum = applyEmit(accum, out);
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
    const nextId = () => st.nextLineId++;
    const emits: MessageEmit[] = [];
    for (const line of lines) {
      if (!line) continue;
      const out = parseEntry(line, st.toolStarts, nextId);
      if (out.append.length > 0 || out.patch.length > 0) emits.push(out);
      // Live tail only: scan for `gh pr …` / `git push` &c and schedule
      // a debounced query refresh. `readSeed` deliberately skips this —
      // replaying hours-old history must not fire refreshes.
      for (const target of detectRefreshTriggers(line)) {
        scheduleTrigger(target);
      }
    }
    // The jsonl grew — `claudeStatus` reads it directly for the row's
    // last-activity age + queue count, so a slug-scoped invalidation
    // here snaps the badge on turn end regardless of whether the line
    // produced a UI-visible emit (system events, queue-ops, etc).
    scheduleSlugChange(st.slug);
    if (emits.length === 0) return;
    this.update(key, (r) => {
      let next: ActionLine[] = r.lines.slice();
      for (const emit of emits) next = applyEmit(next, emit);
      const lines =
        next.length > MAX_BUFFERED_LINES
          ? next.slice(-MAX_BUFFERED_LINES)
          : next;
      return { ...r, lines };
    });
  }

}

/**
 * Apply one parser delta to a snapshot of buffer lines, returning a
 * new array. Patches by id (no-op when the id has already been evicted
 * past `MAX_BUFFERED_LINES` — the user can't see the line anyway),
 * then appends. Single pass over the array per delta; cheap at our
 * buffer scale (1000 lines, a handful of patches per delta).
 */
function applyEmit(prev: readonly ActionLine[], emit: MessageEmit): ActionLine[] {
  const { append, patch } = emit;
  if (append.length === 0 && patch.length === 0) return prev.slice();
  let next: ActionLine[] = prev.slice();
  if (patch.length > 0) {
    const byId = new Map<number, ActionLine>();
    for (const p of patch) byId.set(p.id, p.line);
    next = next.map((l) => byId.get(l.id) ?? l);
  }
  if (append.length > 0) next = [...next, ...append];
  return next;
}

const EMPTY_EMIT: MessageEmit = { append: [], patch: [] };

function parseEntry(
  raw: string,
  toolStarts: ToolStartMap,
  nextId: () => number,
): MessageEmit {
  let evt: unknown;
  try {
    evt = JSON.parse(raw);
  } catch {
    return EMPTY_EMIT;
  }
  const e = asObj(evt);
  if (!e) return EMPTY_EMIT;
  const t = e.type;
  const ts = entryTs(e);
  if (t === "assistant" || t === "user") {
    // The auto-injected post-compaction summary blob ("This session is
    // being continued from a previous conversation…") arrives as a
    // user envelope flagged with `isCompactSummary`. Skip it — the
    // `compact_boundary` system event below carries the high-signal
    // marker (token deltas, trigger), and rendering the full summary
    // would dump several hundred lines of internal detail into the pane.
    if (t === "user" && e.isCompactSummary === true) return EMPTY_EMIT;
    return messageToLines({
      role: t,
      message: e.message,
      ts,
      toolStarts,
      nextId,
    });
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
    return {
      append: [
        {
          id: nextId(),
          ts,
          kind: "info",
          text: `↘ compacted${triggerPart}${tokenPart}`,
        },
      ],
      patch: [],
    };
  }
  // system.away_summary — claude's auto-generated context-recap when
  // the conversation is auto-compacted. High-signal: the user can
  // glance at the pane and see "this is what the previous turns were
  // about" without re-reading the whole tail. Multi-line: same
  // newline-split + per-line cap as assistant text, with the leading
  // `─` only on the first row so the block reads as one summary
  // group rather than a series of dash bullets.
  if (t === "system" && e.subtype === "away_summary") {
    const raw = typeof e.content === "string" ? e.content : "";
    const content = raw.replace(AWAY_RECAP_HINT_RE, "");
    const { pieces, truncated } = splitMessage(content);
    if (pieces.length === 0) return EMPTY_EMIT;
    const lines: ActionLine[] = pieces.map((piece, i) => ({
      id: nextId(),
      ts,
      kind: "info",
      text: `${i === 0 ? "─ " : "  "}${piece}`,
    }));
    if (truncated > 0) {
      lines.push({
        id: nextId(),
        ts,
        kind: "info",
        text: `  …${truncated} more line${truncated === 1 ? "" : "s"} truncated`,
      });
    }
    return { append: lines, patch: [] };
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
        return {
          append: [
            {
              id: nextId(),
              ts,
              kind: "info",
              text: `⏎ queued: ${compacted}`,
            },
          ],
          patch: [],
        };
      }
    }
    return EMPTY_EMIT;
  }
  return EMPTY_EMIT;
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
