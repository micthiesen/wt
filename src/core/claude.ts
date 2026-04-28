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

export type ClaudeStatus = {
  state: ClaudeState;
  /** Total non-empty session count (jsonls with at least one user message). */
  count: number;
  /** Pending queued prompts in the newest session (enqueues minus dequeues). */
  queued: number;
};

const NONE: ClaudeStatus = { state: { kind: "none" }, count: 0, queued: 0 };

const TAIL_BYTES = 16 * 1024;
const WORKING_MAX_AGE_MS = 60_000;
const WAITING_MAX_AGE_MS = 30 * 60_000;
const USER_MARKER = '"type":"user"';

function projectDir(wtPath: string): string {
  return join(homedir(), ".claude", "projects", wtPath.replace(/\//g, "-"));
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

    if (lastMeaningful) {
      const isWorking =
        (lastMeaningful.type === "assistant" &&
          assistantStopReason(lastMeaningful.raw) === "tool_use") ||
        (lastMeaningful.type === "user" && isToolResultUser(lastMeaningful.raw));
      if (isWorking && age < WORKING_MAX_AGE_MS) {
        return { state: { kind: "working", lastEntryMs }, count, queued };
      }
      if (
        lastMeaningful.type === "assistant" &&
        assistantStopReason(lastMeaningful.raw) === "end_turn" &&
        age < WAITING_MAX_AGE_MS
      ) {
        return { state: { kind: "waiting", lastEntryMs }, count, queued };
      }
    }

    if (count > 0) {
      return { state: { kind: "stale", lastEntryMs }, count, queued };
    }
    return NONE;
  } catch {
    return NONE;
  }
}
