/**
 * Persistent "last focused" timestamps for the task-inbox hub, one per
 * worktree slug. The hub stamps a slug's session the moment the user
 * focuses it (switches a pane onto it); a task's session output that
 * arrives AFTER the stamp is "unread" — the hub diffs a task's latest
 * session-activity timestamp against this store to decide whether to
 * show the unread glyph. Persisted (not just in-memory) so unread state
 * survives a `wt` restart instead of re-flagging every task as unread
 * on launch.
 *
 * File shape: `{ "<slug>": <epoch-ms>, ... }` at
 * `~/.cache/wt/task-focus.json`. Kept deliberately dumb — no per-slug
 * TTL logic, no migration versioning — because it's a cache of "when
 * did the user last look", not state anything else derives from.
 *
 * Deliberately does NOT go through `wtstate/io.ts`'s `withWtStateLock`:
 * that lock exists to serialize read-modify-write across MULTIPLE
 * writer processes (CLI mutations racing the TUI), and pulls in
 * `core/config.ts` (which fails fast without a real `config.toml`) to
 * find the lock directory. `record()` is called only from the hub's own
 * process on its own local ui action, so a plain load-once /
 * write-through is enough, and staying config-independent keeps this
 * module trivially unit-testable against a temp path.
 */
import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

import { createLogger } from "./logger.ts";

const log = createLogger("[task-focus]");

/** Entries older than this are dropped on load — unbounded-growth guard for slugs whose worktrees were destroyed long ago and will never be looked up again. */
const MAX_AGE_MS = 30 * 24 * 60 * 60 * 1000;

export type TaskFocusStore = {
  /** Register a listener fired after every `record()`. Returns an unsubscribe function. Shaped for `useSyncExternalStore`. */
  subscribe(cb: () => void): () => void;
  /** Stable-identity snapshot; changes identity only on record(). */
  getSnapshot(): ReadonlyMap<string, number>;
  /** Stamp now for slug and persist. */
  record(slug: string, nowMs?: number): void;
};

function readFile(filePath: string): Map<string, number> {
  if (!existsSync(filePath)) return new Map();
  try {
    const parsed = JSON.parse(readFileSync(filePath, "utf8")) as unknown;
    if (!parsed || typeof parsed !== "object") return new Map();
    const out = new Map<string, number>();
    for (const [slug, ms] of Object.entries(parsed as Record<string, unknown>)) {
      if (typeof ms === "number" && Number.isFinite(ms)) out.set(slug, ms);
    }
    return out;
  } catch (err) {
    // Tolerate a missing/corrupt file — this is a cache, not a source
    // of truth; starting empty just means everything looks "read" once
    // until the user's next focus.
    log.error(err instanceof Error ? err : String(err), { file: filePath });
    return new Map();
  }
}

function writeFile(filePath: string, map: ReadonlyMap<string, number>): void {
  try {
    mkdirSync(dirname(filePath), { recursive: true });
    // Write-then-rename, same pattern as `wtstate/io.ts`'s
    // `writeWtState`: a concurrent reader never observes a
    // half-written file, and rename(2) is atomic within a filesystem.
    const tmp = `${filePath}.${process.pid}.tmp`;
    const shape = Object.fromEntries(map);
    writeFileSync(tmp, `${JSON.stringify(shape, null, 2)}\n`);
    renameSync(tmp, filePath);
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), { file: filePath });
    // Unlike `writeWtState`, don't re-raise: losing a focus stamp just
    // means a task looks unread a little longer, not a corrupted
    // primitive another module depends on.
  }
}

function pruneStale(map: Map<string, number>, nowMs: number): boolean {
  let changed = false;
  const cutoff = nowMs - MAX_AGE_MS;
  for (const [slug, ms] of map) {
    if (ms < cutoff) {
      map.delete(slug);
      changed = true;
    }
  }
  return changed;
}

/**
 * Build an independent store bound to `filePath`. Exported (rather than
 * only the singleton below) so tests can point at a temp file instead
 * of the real `~/.cache/wt/task-focus.json`.
 */
export function createTaskFocusStore(filePath: string): TaskFocusStore {
  let loaded = false;
  let map = new Map<string, number>();
  let snapshot: ReadonlyMap<string, number> = map;
  const listeners = new Set<() => void>();

  function ensureLoaded(): void {
    if (loaded) return;
    loaded = true;
    map = readFile(filePath);
    if (pruneStale(map, Date.now())) writeFile(filePath, map);
    snapshot = new Map(map);
  }

  return {
    subscribe(cb) {
      listeners.add(cb);
      return () => {
        listeners.delete(cb);
      };
    },
    getSnapshot() {
      ensureLoaded();
      return snapshot;
    },
    record(slug, nowMs = Date.now()) {
      ensureLoaded();
      map.set(slug, nowMs);
      // New identity so `useSyncExternalStore` (and any naive
      // `===`-comparing caller) sees the change; reads between
      // `record()` calls keep returning the same reference.
      snapshot = new Map(map);
      writeFile(filePath, map);
      for (const cb of listeners) cb();
    },
  };
}

/** Singleton on `~/.cache/wt/task-focus.json`, the hub's real store. */
export const taskFocusStore: TaskFocusStore = createTaskFocusStore(
  join(homedir(), ".cache", "wt", "task-focus.json"),
);
