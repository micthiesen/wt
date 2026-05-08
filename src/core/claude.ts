import {
  closeSync,
  existsSync,
  openSync,
  readSync,
  statSync,
} from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

import { asObj } from "./claude-events.ts";
import { listClaudeNames } from "./claude-sessions.ts";

/**
 * Last meaningful entry in a session jsonl, classified for the per-
 * session state machine. The row combines this with tmux liveness
 * for `(slug, name)` to derive working / waiting / abandoned / idle —
 * see `tui/rows/claude.tsx`. Mid-turn vs. end-of-turn is the only
 * distinction the row cares about, but the granular kinds make
 * future consumers (CLI status, debugging) easier.
 *
 *   - "tool_use"    — assistant ended with stop_reason="tool_use"
 *                     (mid-turn; expects a tool_result next).
 *   - "tool_result" — user message carried a tool_result block
 *                     (mid-turn; claude just got tool output back).
 *   - "paused"      — assistant ended with stop_reason="pause_turn"
 *                     (mid-turn; extended-thinking pause).
 *   - "end_turn"    — assistant ended with a terminal stop_reason
 *                     (end_turn / max_tokens / stop_sequence /
 *                     refusal). Claude is done writing for now;
 *                     waiting for human input.
 *   - "other"       — entry didn't match any of the above.
 *   - null          — no meaningful entries (jsonl missing or empty).
 */
export type LastEntryKind =
  | "tool_use"
  | "tool_result"
  | "paused"
  | "end_turn"
  | "other"
  | null;

/**
 * Per-session jsonl tail data — purely filesystem-derived. The row
 * pairs each entry with `useClaudeSessionsForSlug(slug)` to know
 * whether the (slug, name) tmux session is currently live and from
 * there derives the displayed state. Keeping derivation in the
 * consumer means tmux-state changes flow through reactively without
 * having to refetch the source.
 */
export type SessionTail = {
  /** `null` = primary; otherwise the user-typed name. */
  name: string | null;
  /** True when the wt-managed jsonl for `(path, name)` exists on disk. */
  hasJsonl: boolean;
  /**
   * Timestamp of the last meaningful entry, or `null` when the jsonl
   * is missing/empty. Used by the row to display age — never as a
   * freshness gate (no heuristic time windows).
   */
  lastEntryMs: number | null;
  lastEntryKind: LastEntryKind;
  /**
   * Pending queued prompts (enqueues − dequeues, clamped at 0).
   * Counted only within the last `TAIL_BYTES` of the jsonl, so a
   * long-running session that has paged earlier enqueues out of
   * the window will under-report. Acceptable: queued counters
   * tend to settle to small numbers within a turn or two.
   */
  queued: number;
};

export type ClaudeStatus = {
  /**
   * Per-session tail data for every wt-managed session in this
   * worktree (primary + persisted names). Order: primary first, then
   * persisted names in `listClaudeNames` order. Sessions with neither
   * a jsonl nor a persisted-name entry are absent — there's nothing
   * to summarize for them.
   */
  sessions: readonly SessionTail[];
};

// Sized to match `SEED_TAIL_BYTES` in `core/session-tail.ts`; large
// assistant turns (multi-tool blocks, sub-agent inlines) regularly
// exceed 16 KiB and would truncate the most-recent envelope's
// `stop_reason` line. 64 KiB is still cheap and covers typical max-
// length turns.
const TAIL_BYTES = 64 * 1024;

/**
 * Claude Code's per-project storage dir. Both `/` and `.` map to `-` —
 * `/Users/michael/.wt` becomes `-Users-michael--wt`. Missing the dot
 * replacement means existsSync always returns false for dot-prefixed
 * paths, so the resume-vs-create gate hands claude `--session-id` for
 * an already-used UUID and it exits immediately. Exported so other
 * modules (session-tail) derive the same dir from the same rule.
 */
export function projectDir(wtPath: string): string {
  return join(homedir(), ".claude", "projects", wtPath.replace(/[/.]/g, "-"));
}

/**
 * Stable namespace for wt-managed conversation UUIDs. Generated once,
 * hardcoded forever — changing it would orphan every existing session.
 */
const WT_SESSION_NAMESPACE = "ad7c39f4-4b63-4d1c-9b9a-66c44e5a1e58";

/**
 * Deterministic UUID for a wt-managed conversation. Primary
 * (`name = null`) keys on `wtPath`; named sessions key on
 * `wtPath + NUL + name` so
 * the same name in two different worktrees never collides, and primary
 * vs. any named version of itself never collides (NUL can't appear in
 * a path or a name). UUIDv5 means same key → same UUID across runs
 * without persisting anything.
 */
export function wtSessionUuid(wtPath: string, name: string | null = null): string {
  const key = name === null ? wtPath : `${wtPath} ${name}`;
  return Bun.randomUUIDv5(key, WT_SESSION_NAMESPACE);
}

/**
 * Path to the on-disk jsonl that backs a session, given its UUID.
 * Used by callers that want to check existence (resume vs. create gate)
 * or watch the file directly.
 */
export function sessionJsonlPath(wtPath: string, uuid: string): string {
  return join(projectDir(wtPath), `${uuid}.jsonl`);
}

/**
 * Args to splice into a `claude` invocation that pin it to a
 * wt-managed conversation. Resumes when the jsonl exists, otherwise
 * creates with our deterministic UUID. `--name` tags the session so
 * `/resume` listings stay readable; `displayName` is the label that
 * shows up there.
 *
 * Resume vs. create is decided by file existence — claude rejects
 * `--session-id` for an already-used ID and `--resume` for a
 * nonexistent one, so the gate is mandatory, not an optimization.
 */
export function wtSessionArgs(opts: {
  wtPath: string;
  /** `null` = primary; otherwise the user-typed session name. */
  name: string | null;
  /** Label shown in claude's `/resume` picker and prompt box. */
  displayName: string;
}): string[] {
  const { wtPath, name, displayName } = opts;
  const uuid = wtSessionUuid(wtPath, name);
  const jsonlPath = sessionJsonlPath(wtPath, uuid);
  const args = ["--name", displayName];
  if (existsSync(jsonlPath)) {
    args.push("--resume", uuid);
  } else {
    args.push("--session-id", uuid);
  }
  return args;
}

function readTail(filePath: string, size: number): string {
  const start = Math.max(0, size - TAIL_BYTES);
  const len = size - start;
  const fd = openSync(filePath, "r");
  try {
    const buf = Buffer.alloc(len);
    readSync(fd, buf, 0, len, start);
    return buf.toString("utf8");
  } finally {
    closeSync(fd);
  }
}

type Entry = { type: string; raw: Record<string, unknown> };

function parseTailLines(tail: string, fileStart: boolean): Entry[] {
  // If we didn't start from byte 0, the first line is likely partial — drop it.
  const lines = tail.split("\n");
  const start = fileStart ? 0 : 1;
  const entries: Entry[] = [];
  for (let i = start; i < lines.length; i++) {
    const line = lines[i];
    if (!line) continue;
    try {
      const obj = asObj(JSON.parse(line));
      if (!obj) continue;
      const type = obj.type;
      if (typeof type === "string") entries.push({ type, raw: obj });
    } catch {
      // skip malformed
    }
  }
  return entries;
}

function entryTimestampMs(raw: Record<string, unknown>): number | null {
  const ts = raw.timestamp;
  if (typeof ts !== "string") return null;
  const t = Date.parse(ts);
  return Number.isNaN(t) ? null : t;
}

function isToolResultUser(raw: Record<string, unknown>): boolean {
  const message = asObj(raw.message);
  if (!message) return false;
  const content = message.content;
  if (!Array.isArray(content)) return false;
  for (const b of content) {
    const obj = asObj(b);
    if (obj && obj.type === "tool_result") return true;
  }
  return false;
}

function assistantStopReason(raw: Record<string, unknown>): string | null {
  const message = asObj(raw.message);
  if (!message) return null;
  const sr = message.stop_reason;
  return typeof sr === "string" ? sr : null;
}

/**
 * Classify the latest meaningful entry walking back from the tail.
 * "Meaningful" filters out non-conversation entries (system events,
 * pure metadata) so the classifier doesn't get distracted by chrome.
 *
 * Stop-reason mapping: `tool_use` and `pause_turn` are mid-turn
 * (claude isn't done with its turn); everything terminal —
 * `end_turn`, `max_tokens`, `stop_sequence`, `refusal` — collapses
 * to `end_turn` since the row only cares about mid-turn vs. not.
 */
function classifyLast(entries: readonly Entry[]): {
  kind: LastEntryKind;
  ts: number | null;
} {
  for (let i = entries.length - 1; i >= 0; i--) {
    const e = entries[i]!;
    if (e.type !== "user" && e.type !== "assistant") continue;
    const ts = entryTimestampMs(e.raw);
    if (e.type === "assistant") {
      const sr = assistantStopReason(e.raw);
      if (sr === "tool_use") return { kind: "tool_use", ts };
      if (sr === "pause_turn") return { kind: "paused", ts };
      // end_turn / max_tokens / stop_sequence / refusal / etc. all
      // mean "claude stopped writing" — fold to end_turn for the row.
      if (sr !== null) return { kind: "end_turn", ts };
      return { kind: "other", ts };
    }
    if (e.type === "user" && isToolResultUser(e.raw)) {
      return { kind: "tool_result", ts };
    }
    return { kind: "other", ts };
  }
  return { kind: null, ts: null };
}

/**
 * Per-path memo of the last `tailSession` result keyed on
 * `(mtimeMs, size)`. The query polls every 5s and most worktrees
 * are idle most of the time — without this, every poll re-parses
 * tens of KB per session even when nothing changed. The cache
 * holds at most O(sessions) entries and is GC-bounded by an LRU
 * cap (the prune below); module-scoped so it survives across
 * queryFn invocations within one process.
 */
type TailCacheEntry = {
  mtimeMs: number;
  size: number;
  result: SessionTail;
};
const tailCache = new Map<string, TailCacheEntry>();
const TAIL_CACHE_MAX = 256;

function emptyTail(name: string | null, hasJsonl: boolean): SessionTail {
  return { name, hasJsonl, lastEntryMs: null, lastEntryKind: null, queued: 0 };
}

function tailSession(wtPath: string, name: string | null): SessionTail {
  const path = sessionJsonlPath(wtPath, wtSessionUuid(wtPath, name));
  let size = 0;
  let mtimeMs = 0;
  try {
    const st = statSync(path);
    if (!st.isFile()) return emptyTail(name, false);
    size = st.size;
    mtimeMs = st.mtimeMs;
  } catch {
    // ENOENT is the common case (no jsonl yet); other errors degrade
    // to the same "nothing to report" rather than rejecting the whole
    // worktree's claudeStatus.
    tailCache.delete(path);
    return emptyTail(name, false);
  }
  if (size === 0) return emptyTail(name, true);

  const cached = tailCache.get(path);
  if (cached && cached.mtimeMs === mtimeMs && cached.size === size) {
    return cached.result;
  }

  // The file may disappear / truncate between stat and read. Treat
  // that the same as "nothing parseable yet" rather than rejecting.
  let result: SessionTail;
  try {
    const tail = readTail(path, size);
    const parsed = parseTailLines(tail, size <= TAIL_BYTES);

    let queued = 0;
    for (const e of parsed) {
      if (e.type !== "queue-operation") continue;
      const op = e.raw.operation;
      if (op === "enqueue") queued++;
      else if (op === "dequeue") queued--;
    }
    if (queued < 0) queued = 0;

    const last = classifyLast(parsed);
    // mtime fallback when the entry has no `timestamp` (rare); it
    // beats returning null for a non-empty jsonl.
    const lastEntryMs = last.ts ?? mtimeMs;
    result = {
      name,
      hasJsonl: true,
      lastEntryMs,
      lastEntryKind: last.kind,
      queued,
    };
  } catch {
    return emptyTail(name, true);
  }

  if (tailCache.size >= TAIL_CACHE_MAX) {
    // Cheap LRU-ish prune: drop the oldest insertion. Map iteration
    // order is insertion order; one delete on overflow keeps memory
    // bounded without per-access bookkeeping.
    const first = tailCache.keys().next().value;
    if (first !== undefined) tailCache.delete(first);
  }
  tailCache.set(path, { mtimeMs, size, result });
  return result;
}

/**
 * Snapshot every wt-managed session in `wt`'s claude project that has
 * a backing jsonl: primary first, then persisted-named sessions in
 * stored order. A persisted name with no jsonl yet (e.g. a freshly-
 * spawned session that hasn't written its first entry, or a stale
 * entry from a spawn-failed name) is filtered out — the row would
 * have nothing real to summarize. State derivation lives in the
 * consumer so tmux liveness (a separate, polled query) flows through
 * reactively without forcing this filesystem source to refetch.
 */
export async function claudeStatus(wt: {
  slug: string;
  path: string;
}): Promise<ClaudeStatus> {
  const sessions: SessionTail[] = [];
  const primary = tailSession(wt.path, null);
  if (primary.hasJsonl) sessions.push(primary);
  for (const name of listClaudeNames(wt.slug)) {
    const tail = tailSession(wt.path, name);
    if (tail.hasJsonl) sessions.push(tail);
  }
  return { sessions };
}
