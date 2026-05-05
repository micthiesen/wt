import {
  closeSync,
  existsSync,
  openSync,
  readFileSync,
  readdirSync,
  readSync,
  statSync,
} from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

export type ClaudeState =
  | { kind: "working"; lastEntryMs: number }
  | { kind: "waiting"; lastEntryMs: number }
  | { kind: "stale"; lastEntryMs: number }
  | { kind: "none" };

/**
 * One-line snapshot of the most recent meaningful event in the newest
 * session. Surfaced in the details pane so the user can see what
 * claude is up to without entering the session.
 *
 * `assistant` carries a one-line excerpt of the latest assistant turn.
 * `tool` carries the active (last-issued, possibly still-running) tool
 * call summary. The details renderer chooses how to format both.
 */
export type ClaudeLatest =
  | { kind: "assistant"; text: string }
  | { kind: "tool"; name: string; arg: string };

export type ClaudeStatus = {
  state: ClaudeState;
  /** Total non-empty session count (jsonls with at least one user message). */
  count: number;
  /** Pending queued prompts in the newest session (enqueues minus dequeues). */
  queued: number;
  /** Latest meaningful event in the newest session, or null when none. */
  latest: ClaudeLatest | null;
};

const NONE: ClaudeStatus = {
  state: { kind: "none" },
  count: 0,
  queued: 0,
  latest: null,
};

const TAIL_BYTES = 16 * 1024;
const WORKING_MAX_AGE_MS = 60_000;
const WAITING_MAX_AGE_MS = 30 * 60_000;
const USER_MARKER = '"type":"user"';

function projectDir(wtPath: string): string {
  // Claude Code normalizes both `/` and `.` to `-` when deriving the
  // project dir name. `/Users/michael/.wt` → `-Users-michael--wt`.
  // Missing the dot replacement here means existsSync always returns
  // false for dot-prefixed paths, so we'd hand claude --session-id for
  // an already-used UUID and it'd exit immediately.
  return join(homedir(), ".claude", "projects", wtPath.replace(/[/.]/g, "-"));
}

/**
 * Stable namespace for wt-managed conversation UUIDs. Generated once,
 * hardcoded forever — changing it would orphan every existing session.
 */
const WT_SESSION_NAMESPACE = "ad7c39f4-4b63-4d1c-9b9a-66c44e5a1e58";

/**
 * Deterministic UUID for the wt-managed conversation in `wtPath`.
 * UUIDv5 over the path → same UUID across runs without persisting
 * anything. Two worktrees at different paths get different IDs even if
 * their slug ever happened to collide.
 */
export function wtSessionUuid(wtPath: string): string {
  return Bun.randomUUIDv5(wtPath, WT_SESSION_NAMESPACE);
}

/**
 * Args to splice into a `claude` invocation that pin it to the
 * wt-managed conversation for `wtPath`. Resumes when the jsonl exists,
 * otherwise creates with our deterministic UUID. `--name wt` tags the
 * session so `/resume` listings stay readable. Resume vs. create is
 * decided by file existence — claude rejects `--session-id` for an
 * already-used ID and `--resume` for a nonexistent one, so the gate
 * is mandatory, not an optimization.
 */
export function wtSessionArgs(wtPath: string): string[] {
  const uuid = wtSessionUuid(wtPath);
  const jsonlPath = join(projectDir(wtPath), `${uuid}.jsonl`);
  const args = ["--name", "wt"];
  if (existsSync(jsonlPath)) {
    args.push("--resume", uuid);
  } else {
    args.push("--session-id", uuid);
  }
  return args;
}

function hasUserEntry(filePath: string): boolean {
  try {
    return readFileSync(filePath, "utf8").includes(USER_MARKER);
  } catch {
    return false;
  }
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
      const obj = JSON.parse(line) as Record<string, unknown>;
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
  const message = raw.message as Record<string, unknown> | undefined;
  const content = message?.content;
  if (!Array.isArray(content)) return false;
  return content.some(
    (b) => b && typeof b === "object" && (b as Record<string, unknown>).type === "tool_result",
  );
}

function assistantStopReason(raw: Record<string, unknown>): string | null {
  const message = raw.message as Record<string, unknown> | undefined;
  const sr = message?.stop_reason;
  return typeof sr === "string" ? sr : null;
}

/** Cap on the assistant-text excerpt we surface in the details pane. */
const LATEST_TEXT_LIMIT = 120;
/** Cap on the tool-arg snippet within a tool latest. */
const LATEST_TOOL_ARG_LIMIT = 60;

function compactLine(s: string, limit: number): string {
  const oneLine = s.replaceAll("\n", " ").replace(/\s+/g, " ").trim();
  return oneLine.length > limit
    ? `${oneLine.slice(0, limit - 1)}…`
    : oneLine;
}

/**
 * Pull the latest assistant text or tool_use from a parsed entry list,
 * walking back from the tail. We prefer assistant text over tool_use
 * (text is more informative when both exist in the same turn), but a
 * lone tool_use is also returned. Returns null when the tail has
 * nothing useful — e.g. only system / queue events.
 *
 * Mirrors the parsing the action runner already does for streamed
 * `claude -p` output, but reads from the on-disk session jsonl so we
 * cover interactive sessions too. Cheap by construction: only the last
 * `TAIL_BYTES` of the newest jsonl are parsed.
 */
function latestFromEntries(entries: readonly Entry[]): ClaudeLatest | null {
  for (let i = entries.length - 1; i >= 0; i--) {
    const e = entries[i]!;
    if (e.type !== "assistant") continue;
    const message = e.raw.message as Record<string, unknown> | undefined;
    const content = message?.content;
    if (!Array.isArray(content)) continue;
    let text: string | null = null;
    let tool: { name: string; arg: string } | null = null;
    for (const block of content) {
      if (!block || typeof block !== "object") continue;
      const b = block as Record<string, unknown>;
      if (b.type === "text" && typeof b.text === "string" && !text) {
        text = compactLine(b.text, LATEST_TEXT_LIMIT);
      } else if (b.type === "tool_use" && !tool) {
        const name = typeof b.name === "string" ? b.name : "?";
        tool = { name, arg: briefToolInput(b.input) };
      }
    }
    if (text) return { kind: "assistant", text };
    if (tool) return { kind: "tool", name: tool.name, arg: tool.arg };
  }
  return null;
}

/**
 * One-line summary of a tool-use input. Mirrors the helper in
 * actions.ts — kept duplicated rather than shared because the action
 * runner reads streamed JSON from a child process whereas we read
 * persisted jsonl, and the two formats may drift independently.
 */
function briefToolInput(input: unknown): string {
  if (!input || typeof input !== "object") return "";
  const obj = input as Record<string, unknown>;
  const keys = [
    "command",
    "file_path",
    "path",
    "pattern",
    "query",
    "url",
    "subagent_type",
    "description",
  ];
  for (const k of keys) {
    const v = obj[k];
    if (typeof v === "string" && v.length > 0) {
      return compactLine(v, LATEST_TOOL_ARG_LIMIT);
    }
  }
  return "";
}

export async function claudeStatus(wt: { path: string }): Promise<ClaudeStatus> {
  try {
    const dir = projectDir(wt.path);
    if (!existsSync(dir)) return NONE;

    let entries: string[];
    try {
      entries = readdirSync(dir);
    } catch {
      return NONE;
    }

    let count = 0;
    let newest: { path: string; mtimeMs: number; size: number } | null = null;
    for (const name of entries) {
      if (!name.endsWith(".jsonl")) continue;
      const full = join(dir, name);
      let st: ReturnType<typeof statSync>;
      try {
        st = statSync(full);
      } catch {
        continue;
      }
      if (!st.isFile() || st.size === 0) continue;
      if (hasUserEntry(full)) count++;
      if (!newest || st.mtimeMs > newest.mtimeMs) {
        newest = { path: full, mtimeMs: st.mtimeMs, size: st.size };
      }
    }

    if (!newest) return NONE;

    const tail = readTail(newest.path, newest.size);
    const fromStart = newest.size <= TAIL_BYTES;
    const parsed = parseTailLines(tail, fromStart);

    let queued = 0;
    for (const e of parsed) {
      if (e.type !== "queue-operation") continue;
      const op = e.raw.operation;
      if (op === "enqueue") queued++;
      else if (op === "dequeue") queued--;
    }
    if (queued < 0) queued = 0;

    let lastMeaningful: Entry | null = null;
    for (let i = parsed.length - 1; i >= 0; i--) {
      const e = parsed[i]!;
      if (e.type === "user" || e.type === "assistant" || e.type === "queue-operation") {
        lastMeaningful = e;
        break;
      }
    }

    const lastEntryMs =
      (lastMeaningful && entryTimestampMs(lastMeaningful.raw)) ?? newest.mtimeMs;
    const age = Date.now() - lastEntryMs;
    const latest = latestFromEntries(parsed);

    if (lastMeaningful) {
      const isWorking =
        (lastMeaningful.type === "assistant" &&
          assistantStopReason(lastMeaningful.raw) === "tool_use") ||
        (lastMeaningful.type === "user" && isToolResultUser(lastMeaningful.raw));
      if (isWorking && age < WORKING_MAX_AGE_MS) {
        return { state: { kind: "working", lastEntryMs }, count, queued, latest };
      }
      if (
        lastMeaningful.type === "assistant" &&
        assistantStopReason(lastMeaningful.raw) === "end_turn" &&
        age < WAITING_MAX_AGE_MS
      ) {
        return { state: { kind: "waiting", lastEntryMs }, count, queued, latest };
      }
    }

    if (count > 0) {
      return { state: { kind: "stale", lastEntryMs }, count, queued, latest };
    }
    return NONE;
  } catch {
    return NONE;
  }
}
