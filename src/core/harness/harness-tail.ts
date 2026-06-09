/**
 * Per-(slug, harness) tail for Codex and OpenCode sessions — the
 * non-claude analogue of `core/session-tail.ts`. Produces the same
 * `ActionLine[]` shape the claude tailer and action runner produce, so
 * the bottom pane (OutputViewer / footer) renders codex/opencode
 * sessions with the exact same row components.
 *
 * Claude gets a purpose-built fs.watch tailer over its stream-json
 * jsonl. Codex and OpenCode persist elsewhere — Codex in a rollout
 * jsonl under `~/.codex/sessions/`, OpenCode in a SQLite DB — and have
 * no per-line push signal, so this registry just polls each live slot
 * on a shared interval, seeding history on first sight and appending
 * deltas after. The event-pane pollers (`codex-events.ts` /
 * `opencode-events.ts`) stay as-is: they emit terse global one-liners
 * and skip history, which is a different job from this detailed,
 * history-seeded per-session trail.
 *
 * Single tmux slot per slug per harness (`<slug>-codex` /
 * `<slug>-opencode`), so the registry key is `${slug}:${harnessId}` and
 * there's at most one run per pair.
 */
import { statSync } from "node:fs";

import type { Statement } from "bun:sqlite";

import {
  type ActionLine,
  type ActionLineKind,
  MAX_BUFFERED_LINES,
} from "../claude-events.ts";
import { createLogger } from "../logger.ts";
import { jsonlTimestamp, readFileSlice } from "../tail-util.ts";

import { latestRolloutForCwd } from "./codex.ts";
import { openDb } from "./opencode.ts";
import type { HarnessId } from "./types.ts";

const log = createLogger("[harness-tail]");

/** Harnesses this registry tails. Claude has its own (`session-tail.ts`). */
export type TailHarnessId = Extract<HarnessId, "codex" | "opencode">;

export type HarnessRun = {
  slug: string;
  harnessId: TailHarnessId;
  startedAt: number;
  lines: readonly ActionLine[];
};

/** One live slot to keep tailed. */
export type LiveHarnessSlot = {
  slug: string;
  wtPath: string;
  harnessId: TailHarnessId;
};

/** Composite registry key. Mirrors the single-slot tmux name scheme. */
export function harnessTailKey(slug: string, harnessId: TailHarnessId): string {
  return `${slug}:${harnessId}`;
}

/** Poll cadence — matches the event-pane pollers. */
const POLL_INTERVAL_MS = 2_500;
/** Trailing bytes of a codex rollout to seed history from. */
const CODEX_SEED_BYTES = 48 * 1024;
/** Cap on parsed lines per single message/output so one giant blob can't
 *  swamp the buffer. */
const MAX_LINES_PER_BLOCK = 8;
/** Per-line character cap before ellipsis (the row truncates by width too,
 *  but bounding here keeps the buffer small). */
const MAX_LINE_CHARS = 240;
/** How many trailing OpenCode parts to seed from on first sight. */
const OPENCODE_SEED_PARTS = 120;

// ---------------------------------------------------------------------------
// Per-entry tail state
// ---------------------------------------------------------------------------

type CodexCursor = {
  /** Rollout path currently tracked, or null until first found. */
  path: string | null;
  /** Byte offset already consumed. */
  offset: number;
  /** Trailing partial line carried to the next read (byte-accurate tail). */
  pending: string;
  /** Drop the first line of the seed window once (it's a partial). */
  seedDrop: boolean;
};

type OpencodeCursor = {
  /** Session id currently tracked (most-recent in the slot's dir). */
  sessionId: string | null;
  /** Max `time_created` consumed so far, ms-since-epoch. */
  afterMs: number;
};

type Entry = {
  slug: string;
  wtPath: string;
  harnessId: TailHarnessId;
  startedAt: number;
  /** Monotonic per-entry line id (same role as the claude tailer's). */
  nextLineId: number;
  /** True once the history seed has run. */
  seeded: boolean;
  codex: CodexCursor;
  opencode: OpencodeCursor;
};

// ---------------------------------------------------------------------------
// Small line helpers
// ---------------------------------------------------------------------------

function clip(s: string): string {
  const t = s.replace(/\s+$/u, "");
  return t.length > MAX_LINE_CHARS ? `${t.slice(0, MAX_LINE_CHARS - 1)}…` : t;
}

/** Split a (possibly multi-line) blob into capped ActionLines. */
function textLines(
  text: string,
  kind: ActionLineKind,
  ts: number,
  nextId: () => number,
  prefix = "",
): ActionLine[] {
  const pieces = text
    .split("\n")
    .map((p) => p.trim())
    .filter((p) => p.length > 0);
  if (pieces.length === 0) return [];
  const out: ActionLine[] = [];
  const shown = pieces.slice(0, MAX_LINES_PER_BLOCK);
  shown.forEach((piece, i) => {
    const lead = i === 0 ? prefix : prefix ? "  " : "";
    out.push({ id: nextId(), ts, kind, text: clip(`${lead}${piece}`) });
  });
  const hidden = pieces.length - shown.length;
  if (hidden > 0) {
    out.push({
      id: nextId(),
      ts,
      kind: "info",
      text: `  …${hidden} more line${hidden === 1 ? "" : "s"}`,
    });
  }
  return out;
}

function oneLine(
  text: string,
  kind: ActionLineKind,
  ts: number,
  nextId: () => number,
): ActionLine {
  return { id: nextId(), ts, kind, text: clip(text) };
}

// ---------------------------------------------------------------------------
// Codex rollout parsing
// ---------------------------------------------------------------------------

/** Map one parsed codex rollout event to zero or more ActionLines. */
function codexEventLines(
  obj: Record<string, unknown>,
  nextId: () => number,
): ActionLine[] {
  const ts = jsonlTimestamp(obj);
  const type = obj.type;

  if (type === "event_msg") {
    const p = obj.payload;
    if (typeof p !== "object" || p === null) return [];
    const pl = p as Record<string, unknown>;
    switch (pl.type) {
      case "user_message": {
        const m = pl.message;
        return typeof m === "string"
          ? textLines(m, "user", ts, nextId, "› ")
          : [];
      }
      case "agent_message": {
        const m = pl.message;
        return typeof m === "string" ? textLines(m, "assistant", ts, nextId) : [];
      }
      case "web_search_end": {
        const q = pl.query;
        return typeof q === "string"
          ? [oneLine(`⚒ web: ${q}`, "tool", ts, nextId)]
          : [];
      }
      case "turn_aborted":
        return [oneLine("⊘ turn interrupted", "info", ts, nextId)];
      default:
        return [];
    }
  }

  if (type === "response_item") {
    const p = obj.payload;
    if (typeof p !== "object" || p === null) return [];
    const pl = p as Record<string, unknown>;
    if (pl.type === "function_call") {
      const name = typeof pl.name === "string" ? pl.name : "tool";
      if (name === "exec_command" || name === "shell") {
        const cmd = extractCodexCmd(pl.arguments);
        return [oneLine(`⚒ ${cmd}`, "tool", ts, nextId)];
      }
      if (name === "apply_patch") {
        return [oneLine("⚒ apply_patch", "tool", ts, nextId)];
      }
      return [oneLine(`⚒ ${name}`, "tool", ts, nextId)];
    }
    if (pl.type === "reasoning") {
      // Codex reasoning summaries land in `summary[].text`. Surface the
      // first as a dim thinking line; full chain-of-thought is noise.
      const summary = pl.summary;
      if (Array.isArray(summary) && summary.length > 0) {
        const first = summary[0] as Record<string, unknown> | undefined;
        const txt = first && typeof first.text === "string" ? first.text : null;
        if (txt) return textLines(txt, "thinking", ts, nextId, "… ").slice(0, 1);
      }
      return [];
    }
  }
  return [];
}

function extractCodexCmd(args: unknown): string {
  if (typeof args !== "string") return "<command>";
  try {
    const parsed = JSON.parse(args) as Record<string, unknown>;
    const raw = parsed.cmd ?? parsed.command;
    if (Array.isArray(raw)) return raw.join(" ");
    if (typeof raw === "string") return raw;
  } catch {
    // fall through
  }
  return args;
}

/**
 * Pull new ActionLines for a codex slot, advancing the byte cursor.
 * First sight seeds from a trailing window; later calls read the delta.
 * Partial trailing lines are held in `cur.pending` across ticks so the
 * offset always advances to EOF without ever re-reading or losing bytes.
 */
function codexPump(entry: Entry): ActionLine[] {
  const rollout = latestRolloutForCwd(entry.wtPath);
  if (!rollout) return [];
  const cur = entry.codex;
  const nextId = () => entry.nextLineId++;

  // First sight, or codex rotated to a new rollout: baseline the cursor.
  // On the first seed we start a trailing window back (and drop its
  // leading partial line); on a mid-run rotation we start at byte 0 of
  // the small fresh file so its opening turn isn't missed.
  if (cur.path !== rollout.path) {
    cur.path = rollout.path;
    cur.pending = "";
    cur.offset = entry.seeded ? 0 : Math.max(0, rollout.size - CODEX_SEED_BYTES);
    cur.seedDrop = cur.offset > 0;
  }

  let size: number;
  try {
    size = statSync(rollout.path).size;
  } catch {
    return [];
  }
  if (size < cur.offset) {
    // Truncated/rotated under us — resync.
    cur.offset = size;
    cur.pending = "";
    return [];
  }
  if (size === cur.offset) return [];

  let body: string;
  try {
    body = readFileSlice(rollout.path, cur.offset, size - cur.offset);
  } catch {
    return [];
  }
  cur.offset = size;

  const combined = cur.pending + body;
  const lines = combined.split("\n");
  cur.pending = lines.pop() ?? ""; // trailing partial → next tick
  const out: ActionLine[] = [];
  for (let i = 0; i < lines.length; i++) {
    if (i === 0 && cur.seedDrop) {
      cur.seedDrop = false; // the seed window's leading partial line
      continue;
    }
    const line = lines[i];
    if (!line || !line.trim()) continue;
    let obj: Record<string, unknown>;
    try {
      obj = JSON.parse(line) as Record<string, unknown>;
    } catch {
      continue;
    }
    out.push(...codexEventLines(obj, nextId));
  }
  return out;
}

// ---------------------------------------------------------------------------
// OpenCode SQLite parsing
// ---------------------------------------------------------------------------

type LatestSessionRow = { id: string };
type TailPartRow = {
  id: string;
  time_created: number;
  ptype: string | null;
  pdata: string | null;
  role: string | null;
};

type OcStmts = {
  latestSession: Statement<LatestSessionRow, [{ $dir: string }]>;
  partsAfter: Statement<TailPartRow, [{ $sid: string; $after: number }]>;
  seedParts: Statement<TailPartRow, [{ $sid: string; $limit: number }]>;
};

let ocStmts: OcStmts | null = null;

function ensureOcStmts(): OcStmts | null {
  if (ocStmts) return ocStmts;
  const db = openDb();
  if (!db) return null;
  try {
    ocStmts = {
      latestSession: db.query<LatestSessionRow, { $dir: string }>(
        `SELECT id FROM session
         WHERE directory = $dir AND time_archived IS NULL
         ORDER BY time_updated DESC LIMIT 1`,
      ) as unknown as Statement<LatestSessionRow, [{ $dir: string }]>,
      partsAfter: db.query<TailPartRow, { $sid: string; $after: number }>(
        `SELECT p.id AS id,
                p.time_created AS time_created,
                json_extract(p.data,'$.type') AS ptype,
                p.data AS pdata,
                json_extract(m.data,'$.role') AS role
         FROM part p JOIN message m ON m.id = p.message_id
         WHERE p.session_id = $sid AND p.time_created > $after
         ORDER BY p.time_created ASC`,
      ) as unknown as Statement<TailPartRow, [{ $sid: string; $after: number }]>,
      seedParts: db.query<TailPartRow, { $sid: string; $limit: number }>(
        `SELECT id, time_created, ptype, pdata, role FROM (
           SELECT p.id AS id,
                  p.time_created AS time_created,
                  json_extract(p.data,'$.type') AS ptype,
                  p.data AS pdata,
                  json_extract(m.data,'$.role') AS role
           FROM part p JOIN message m ON m.id = p.message_id
           WHERE p.session_id = $sid
           ORDER BY p.time_created DESC LIMIT $limit
         ) ORDER BY time_created ASC`,
      ) as unknown as Statement<TailPartRow, [{ $sid: string; $limit: number }]>,
    };
    return ocStmts;
  } catch (err) {
    log.warn("opencode tail: prepare stmts failed", { err: String(err) });
    return null;
  }
}

function opencodePartLines(row: TailPartRow, nextId: () => number): ActionLine[] {
  const ts = row.time_created;
  if (!row.pdata) return [];
  let data: Record<string, unknown>;
  try {
    data = JSON.parse(row.pdata) as Record<string, unknown>;
  } catch {
    return [];
  }
  switch (row.ptype) {
    case "text": {
      const text = typeof data.text === "string" ? data.text : "";
      if (!text.trim()) return [];
      return row.role === "user"
        ? textLines(text, "user", ts, nextId, "› ")
        : textLines(text, "assistant", ts, nextId);
    }
    case "reasoning": {
      const text = typeof data.text === "string" ? data.text : "";
      return text.trim()
        ? textLines(text, "thinking", ts, nextId, "… ").slice(0, 1)
        : [];
    }
    case "tool": {
      const tool =
        typeof data.tool === "string"
          ? data.tool
          : typeof data.name === "string"
            ? data.name
            : "tool";
      const state = data.state as Record<string, unknown> | undefined;
      const status =
        state && typeof state.status === "string" ? state.status : null;
      const title =
        state && typeof state.title === "string" ? ` ${state.title}` : "";
      const kind: ActionLineKind =
        status === "error"
          ? "tool-err"
          : status === "completed"
            ? "tool-ok"
            : "tool";
      return [oneLine(`⚒ ${tool}${title}`, kind, ts, nextId)];
    }
    case "patch":
      return [oneLine("⚒ patch", "tool", ts, nextId)];
    default:
      return [];
  }
}

function opencodePump(entry: Entry): ActionLine[] {
  const s = ensureOcStmts();
  if (!s) return [];
  const nextId = () => entry.nextLineId++;
  const cur = entry.opencode;

  let session: LatestSessionRow | null;
  try {
    session = s.latestSession.get({ $dir: entry.wtPath }) ?? null;
  } catch {
    return [];
  }
  if (!session) return [];

  // New slot session (fresh spawn or first sight): reset the cursor.
  const isNewSession = cur.sessionId !== session.id;
  if (isNewSession) {
    cur.sessionId = session.id;
    cur.afterMs = 0;
  }

  let rows: TailPartRow[];
  try {
    rows =
      !entry.seeded || isNewSession
        ? s.seedParts.all({ $sid: session.id, $limit: OPENCODE_SEED_PARTS })
        : s.partsAfter.all({ $sid: session.id, $after: cur.afterMs });
  } catch (err) {
    log.debug("opencode tail query failed", {
      slug: entry.slug,
      err: String(err),
    });
    return [];
  }

  const out: ActionLine[] = [];
  for (const row of rows) {
    if (row.time_created > cur.afterMs) cur.afterMs = row.time_created;
    out.push(...opencodePartLines(row, nextId));
  }
  return out;
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

type Listener = () => void;

class HarnessTailRegistry {
  private runs: ReadonlyMap<string, HarnessRun> = new Map();
  private state = new Map<string, Entry>();
  private listeners = new Set<Listener>();
  private poller: ReturnType<typeof setInterval> | null = null;

  ensure(slug: string, wtPath: string, harnessId: TailHarnessId): void {
    const key = harnessTailKey(slug, harnessId);
    const existing = this.state.get(key);
    if (existing) {
      // Re-point if the worktree path changed under the same slug.
      if (existing.wtPath === wtPath) return;
      this.stopByKey(key);
    }
    const entry: Entry = {
      slug,
      wtPath,
      harnessId,
      startedAt: Date.now(),
      nextLineId: 1,
      seeded: false,
      codex: { path: null, offset: 0, pending: "", seedDrop: false },
      opencode: { sessionId: null, afterMs: 0 },
    };
    this.state.set(key, entry);
    this.commit((m) =>
      m.set(key, { slug, harnessId, startedAt: entry.startedAt, lines: [] }),
    );
    // Seed synchronously so the pane shows history the moment it opens.
    this.pump(key);
    this.ensurePoller();
  }

  stop(slug: string, harnessId: TailHarnessId): void {
    this.stopByKey(harnessTailKey(slug, harnessId));
  }

  private stopByKey(key: string): void {
    if (!this.state.delete(key)) return;
    this.commit((m) => {
      m.delete(key);
    });
    if (this.state.size === 0) this.stopPoller();
  }

  /** Spin tailers for the live set; drop tailers no longer live. */
  reconcile(live: readonly LiveHarnessSlot[]): void {
    const liveKeys = new Set<string>();
    for (const slot of live) {
      liveKeys.add(harnessTailKey(slot.slug, slot.harnessId));
      this.ensure(slot.slug, slot.wtPath, slot.harnessId);
    }
    for (const key of [...this.state.keys()]) {
      if (!liveKeys.has(key)) this.stopByKey(key);
    }
  }

  stopAll(): void {
    for (const key of [...this.state.keys()]) this.stopByKey(key);
    this.stopPoller();
  }

  getSnapshot = (): ReadonlyMap<string, HarnessRun> => this.runs;

  subscribe = (fn: Listener): (() => void) => {
    this.listeners.add(fn);
    return () => {
      this.listeners.delete(fn);
    };
  };

  // ---------- internals ----------

  private ensurePoller(): void {
    if (this.poller) return;
    this.poller = setInterval(() => {
      for (const key of this.state.keys()) this.pump(key);
    }, POLL_INTERVAL_MS);
  }

  private stopPoller(): void {
    if (!this.poller) return;
    clearInterval(this.poller);
    this.poller = null;
  }

  private pump(key: string): void {
    const entry = this.state.get(key);
    if (!entry) return;
    let appended: ActionLine[];
    try {
      appended =
        entry.harnessId === "codex" ? codexPump(entry) : opencodePump(entry);
    } catch (err) {
      log.warn("harness tail pump failed", {
        slug: entry.slug,
        harness: entry.harnessId,
        err: String(err),
      });
      appended = [];
    }
    entry.seeded = true;
    if (appended.length === 0) return;
    const cur = this.runs.get(key);
    if (!cur) return;
    const next = [...cur.lines, ...appended];
    const trimmed =
      next.length > MAX_BUFFERED_LINES ? next.slice(-MAX_BUFFERED_LINES) : next;
    this.commit((m) => m.set(key, { ...cur, lines: trimmed }));
  }

  private commit(mut: (m: Map<string, HarnessRun>) => void): void {
    const next = new Map(this.runs);
    mut(next);
    this.runs = next;
    for (const l of this.listeners) {
      try {
        l();
      } catch {
        // sink errors must not break dispatch
      }
    }
  }
}

export const harnessTailRegistry = new HarnessTailRegistry();
