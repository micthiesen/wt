/**
 * Shared plumbing for the file-tailing modules (session-tail,
 * action-tail, shell-tail, harness-tail, the codex rollout readers,
 * and claude-summaries). Each of these grew an identical private copy
 * of the byte-range reader / watcher closer / jsonl-timestamp helper;
 * they live here so the load-bearing tail logic can't drift.
 */
import { closeSync, openSync, readSync, type FSWatcher } from "node:fs";

/**
 * Read `len` bytes at byte offset `start` from an open fd, decoded as
 * UTF-8. `readSync` can return short under signal interrupts — slice
 * to actual bytes-read so the decoder doesn't see the zero-filled
 * buffer tail as UTF-8 NUL bytes.
 */
export function readFdSlice(fd: number, start: number, len: number): string {
  const buf = Buffer.alloc(len);
  const n = readSync(fd, buf, 0, len, start);
  return buf.toString("utf8", 0, n);
}

/**
 * Read `len` bytes at byte offset `start` of `path`. Opens and closes
 * its own fd; throws when the file can't be opened (callers that poll
 * wrap in try/catch and skip the tick).
 */
export function readFileSlice(path: string, start: number, len: number): string {
  const fd = openSync(path, "r");
  try {
    return readFdSlice(fd, start, len);
  } finally {
    closeSync(fd);
  }
}

/** Best-effort close of an fs.watch watcher. */
export function closeSilent(w: FSWatcher | null): void {
  if (!w) return;
  try {
    w.close();
  } catch {
    // best-effort
  }
}

/**
 * Timestamp of a claude/codex jsonl envelope: the `timestamp` field
 * when present and parseable, else "now" (an entry with no usable
 * stamp still has to sort somewhere — treat it as fresh).
 */
export function jsonlTimestamp(obj: Record<string, unknown>): number {
  const ts = obj.timestamp;
  if (typeof ts === "string") {
    const parsed = Date.parse(ts);
    if (!Number.isNaN(parsed)) return parsed;
  }
  return Date.now();
}
