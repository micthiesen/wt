/**
 * Shared on-disk contract between the `wt events` webhook daemon and the
 * TUI. Three artifacts under `<cacheDir>/events/`:
 *
 *   - `github.json`  — the daemon's warm snapshot of `GithubData` (PRs +
 *     merge queue) plus the branch set it covers and a write timestamp.
 *   - `github.touch` — a marker the TUI fs-watches; the daemon rewrites it
 *     after every snapshot so a running TUI invalidates `["github"]`
 *     immediately, exactly like the `.git/refs/` watcher does for commits.
 *   - `state.json`   — daemon liveness + last-event/last-fetch timestamps,
 *     read by `wt events status`.
 *
 * Deliberately NOT the TUI's `cache.sqlite`: that DB is owned by the
 * TanStack persister (one writer, every query settle). A second writer
 * invites WAL contention, so the daemon keeps its own files and the TUI
 * reads them through the normal query path.
 */
import { mkdirSync, readFileSync, renameSync, watch, writeFileSync, type FSWatcher } from "node:fs";
import { dirname, join } from "node:path";

import { config } from "../config.ts";
import { createLogger } from "../logger.ts";
import { makeDebounced } from "../repo-watch.ts";
import { closeSilent } from "../tail-util.ts";
import type { MergeQueueEntry, PullRequest } from "../types.ts";

const log = createLogger("[events]");

/** Events dir sits beside the persisted query cache so it follows a moved `cache_db`. */
export const EVENTS_DIR = join(dirname(config.paths.cacheDb), "events");
export const SNAPSHOT_NAME = "github.json";
export const SNAPSHOT_PATH = join(EVENTS_DIR, SNAPSHOT_NAME);
export const MARKER_NAME = "github.touch";
export const MARKER_PATH = join(EVENTS_DIR, MARKER_NAME);
export const STATE_PATH = join(EVENTS_DIR, "state.json");

/**
 * How recent a snapshot must be for the TUI to serve it in place of a
 * live `gh` fetch. Sized to cover the post-delivery window (marker →
 * invalidate → queryFn is sub-second) without letting an idle snapshot
 * shadow the staleTime backstop: once the snapshot ages past this, the
 * backstop poll does a real fetch, so a missed webhook delivery can't
 * pin stale data for longer than `backstopPollMs`.
 */
const SNAPSHOT_FRESH_MS = 90_000;

/** Serialized form of the github query result (Record, not Map). */
export type GithubSnapshotData = {
  prs: Record<string, PullRequest>;
  mergeQueue: Record<string, MergeQueueEntry>;
};

export type GithubSnapshot = GithubSnapshotData & {
  /** `Date.now()` at write time. */
  updatedAt: number;
  /** The worktree branch set this snapshot was fetched for. */
  branches: string[];
};

export type EventsState = {
  pid: number;
  port: number;
  startedAt: number;
  /** Last accepted (signature-valid, relevant) webhook delivery. */
  lastEventAt: number | null;
  /** Last successful snapshot write. */
  lastFetchAt: number | null;
  /** Accepted deliveries since start. */
  eventCount: number;
  /** Last fetch error message, if any. */
  lastError: string | null;
};

export function ensureEventsDir(): void {
  mkdirSync(EVENTS_DIR, { recursive: true });
}

function writeAtomic(path: string, text: string): void {
  const tmp = `${path}.tmp-${process.pid}`;
  writeFileSync(tmp, text);
  renameSync(tmp, path);
}

export function writeSnapshot(snap: GithubSnapshot): void {
  ensureEventsDir();
  writeAtomic(SNAPSHOT_PATH, JSON.stringify(snap));
}

/** Rewrite the marker so a watching TUI fires. Content is the timestamp (for debugging only). */
export function touchMarker(ts: number): void {
  ensureEventsDir();
  writeAtomic(MARKER_PATH, String(ts));
}

export function readSnapshot(): GithubSnapshot | null {
  try {
    const raw = readFileSync(SNAPSHOT_PATH, "utf8");
    const parsed = JSON.parse(raw) as Partial<GithubSnapshot>;
    if (
      typeof parsed?.updatedAt !== "number" ||
      !Array.isArray(parsed.branches) ||
      !parsed.prs ||
      !parsed.mergeQueue
    ) {
      return null;
    }
    return parsed as GithubSnapshot;
  } catch {
    // Missing / mid-write / malformed — caller falls back to a live fetch.
    return null;
  }
}

/**
 * Serve the github query from the daemon's snapshot when it's fresh and
 * covers every requested branch; otherwise return null so the caller does
 * a live `gh` fetch. Coverage is exact-subset: a branch absent from the
 * snapshot's set means the daemon hasn't fetched it yet (worktree just
 * added), which is indistinguishable from "branch has no PR" without the
 * explicit `branches` list — so we fall back rather than report a missing
 * PR.
 */
export function snapshotForBranches(
  branches: readonly string[],
): GithubSnapshotData | null {
  const snap = readSnapshot();
  if (!snap) return null;
  if (Date.now() - snap.updatedAt > SNAPSHOT_FRESH_MS) return null;
  const covered = new Set(snap.branches);
  for (const b of branches) if (!covered.has(b)) return null;
  const prs: Record<string, PullRequest> = {};
  for (const b of branches) {
    const pr = snap.prs[b];
    if (pr) prs[b] = pr;
  }
  // Merge-queue entries aren't branch-scoped at fetch time (the query
  // pulls the whole queue), so pass them through verbatim to match a live
  // fetch — the TUI keys them by head branch and ignores non-displayed ones.
  return { prs, mergeQueue: snap.mergeQueue };
}

export function writeState(state: EventsState): void {
  ensureEventsDir();
  writeAtomic(STATE_PATH, JSON.stringify(state, null, 2));
}

export function readState(): EventsState | null {
  try {
    const parsed = JSON.parse(readFileSync(STATE_PATH, "utf8")) as Partial<EventsState>;
    if (typeof parsed?.pid !== "number" || typeof parsed.port !== "number") return null;
    return parsed as EventsState;
  } catch {
    return null;
  }
}

/** True when a pid is a live process (signal 0 probe). */
export function isProcessAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

/**
 * Watch the events dir for marker rewrites and invalidate on each. Mirrors
 * `watchRefs` in `core/repo-watch.ts`: debounced, error-tolerant, degrades
 * to the polling backstop on setup failure. Watches the directory (not the
 * file) because atomic rename-replace breaks a single-file watch on macOS.
 */
export function watchGithubEvents(onChange: () => void): () => void {
  ensureEventsDir();
  const debounced = makeDebounced(onChange, 200);
  let watcher: FSWatcher | null = null;
  try {
    watcher = watch(EVENTS_DIR, { persistent: false }, (_event, filename) => {
      // Fire on the snapshot OR the marker. Critically, match by PREFIX:
      // `writeAtomic` writes `github.touch.tmp-<pid>` then renames it into
      // place, and Bun's fs.watch reports the rename event under the *temp*
      // source name (`github.touch.tmp-88032`), not the final `github.touch`.
      // An exact `=== MARKER_NAME` check therefore never matched and the
      // marker watch was silently dead — the TUI only refreshed via the slow
      // backstop poll. `startsWith` catches both the temp and final names, and
      // both files (`github.json`/`github.touch`). The rename only completes —
      // and the event only fires — once the full document is on disk, so this
      // never reads a half-written file. `filename` can also be null on some
      // macOS event types — fire then too. `state.json` and the daemon logs
      // don't share these prefixes, so per-event state writes don't over-fire.
      if (
        filename == null ||
        filename.startsWith(MARKER_NAME) ||
        filename.startsWith(SNAPSHOT_NAME)
      ) {
        debounced.trigger();
      }
    });
    watcher.on("error", (err) => {
      log.warn("events watcher error", { err: String(err), dir: EVENTS_DIR });
    });
  } catch (err) {
    log.warn("events watcher failed", { err: String(err), dir: EVENTS_DIR });
    return () => debounced.cancel();
  }
  return () => {
    debounced.cancel();
    closeSilent(watcher);
  };
}
