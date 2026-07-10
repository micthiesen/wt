/**
 * Filesystem watchers that turn local git activity into query
 * invalidations — a small "push" floor under the existing pull / poll
 * model. Press-`r` becomes a backstop instead of the primary way to see
 * a fresh commit, push, or working-tree edit.
 *
 * Four watch points:
 *   1. `<main>/.git/refs/` recursive — branch + remote ref churn.
 *      Catches commits (refs/heads/*), fetches + pushes (refs/remotes/*),
 *      branch creates / deletes. Shared across the whole repo, one
 *      subscription.
 *   2. Per-worktree dir recursive — working-tree edits (dirty flips) and
 *      `.sst/` writes (deploy-badge flips). `.git/` events are filtered
 *      out so ref activity doesn't double-fire here; refs come through
 *      (1).
 *   3. `<main>/.git/worktrees/` non-recursive — worktree add/remove
 *      (each worktree is a subdir there). Catches `wt new` / `wt rm` /
 *      `/split` run from a shell or a Claude session, plus the detached
 *      destroy's `git worktree remove`, so the list updates without `r`.
 *      Deliberately non-recursive: per-worktree admin files (index.lock,
 *      HEAD) churn constantly during normal git use and only the
 *      membership set matters here.
 *   4. `~/.cache/wt/` non-recursive, filtered to `state.json` /
 *      `archive.json` — cross-process state writes (CLI `wt stack`
 *      ops, `wt base set`, the detached destroy, another wt instance).
 *      Prefix-matched because both writers go through a
 *      `<file>.<pid>.tmp` → rename dance (see the marker watcher in
 *      events/store.ts for the same trick).
 *
 * Both paths debounce since FSEvents bursts. ENOENT or setup errors log
 * and fall through to the polling backstop on each query — never crash
 * the TUI. macOS-only by virtue of `fs.watch({ recursive: true })`;
 * other platforms degrade to polling, same as the rest of wt.
 *
 * Note: branch switches inside a worktree (`git checkout other-branch`)
 * touch the worktree's `HEAD` under `<main>/.git/worktrees/<slug>/`, not
 * `refs/`. They DO churn the working tree though (files reappear), so
 * the per-worktree dir watcher catches the visible side and the dirty
 * query invalidates. Other per-wt queries (sync, gitActivity, merged,
 * gone) stay on their staleTimes until the next ref-touching action.
 * Good enough for v1; a dedicated HEAD watcher can layer on later if it
 * becomes painful.
 */
import { mkdirSync, watch, type FSWatcher } from "node:fs";
import { join } from "node:path";

import { createLogger } from "./logger.ts";
import { closeSilent } from "./tail-util.ts";
import { WT_STATE_DIR } from "./wtstate.ts";

const log = createLogger("[repo-watch]");

/** Coalesce ref bursts — a `git fetch` writes many remote refs in
 *  quick succession; one debounced invalidation pass per burst is
 *  enough. */
const REFS_DEBOUNCE_MS = 300;

/** Editors, formatters, and build chains burst hard on save. Wider
 *  window than refs so we don't refetch dirty mid-burst. */
const WT_DEBOUNCE_MS = 500;

export type Debounced = { trigger: () => void; cancel: () => void };

/**
 * Trailing-edge debounce shared by the fs watchers in this module and the
 * github-events marker watcher (`core/events/store.ts`) — FSEvents bursts,
 * one invalidation pass per burst is enough.
 */
export function makeDebounced(onChange: () => void, ms: number): Debounced {
  let timer: ReturnType<typeof setTimeout> | null = null;
  let disposed = false;
  return {
    trigger: () => {
      if (disposed) return;
      if (timer !== null) clearTimeout(timer);
      timer = setTimeout(() => {
        timer = null;
        if (!disposed) onChange();
      }, ms);
    },
    cancel: () => {
      disposed = true;
      if (timer !== null) {
        clearTimeout(timer);
        timer = null;
      }
    },
  };
}

/**
 * Subscribe to ref activity in the main clone's `.git/refs/` tree.
 * Recursive — one FSEvents stream covers every branch and remote ref.
 * Caller invalidates the relevant queries on each tick.
 */
export function watchRefs(
  mainClonePath: string,
  onChange: () => void,
): () => void {
  const dir = join(mainClonePath, ".git", "refs");
  const debounced = makeDebounced(onChange, REFS_DEBOUNCE_MS);
  let watcher: FSWatcher | null = null;
  try {
    watcher = watch(dir, { persistent: false, recursive: true }, () =>
      debounced.trigger(),
    );
    watcher.on("error", (err) => {
      log.warn("refs watcher error", { err: String(err), dir });
    });
  } catch (err) {
    log.warn("refs watcher failed", { err: String(err), dir });
    return () => debounced.cancel();
  }
  return () => {
    debounced.cancel();
    closeSilent(watcher);
  };
}

/**
 * Which part of a worktree an fs event landed in, for scoping the
 * resulting invalidation:
 *  - `"tree"` — a working-tree edit → dirty query.
 *  - `"sst"`  — anything under `.sst/` (stage pin, outputs.json, sst's
 *    own logs) → deploy query. Deploys/removes run inside the worktree
 *    (actions, Claude sessions, a shell) always write here, so this is
 *    the push signal that flips the deploy badge without `r`.
 */
export type WorktreeDirArea = "tree" | "sst";

/**
 * Subscribe to working-tree edits in one worktree. Recursive watch on
 * the path; `.git/*` events are filtered out (refs already come through
 * the dedicated refs watcher, and the worktree's `.git` is a file
 * pointer anyway). Events are classified into a `WorktreeDirArea` and
 * debounced per-area so an `.sst/` burst during a deploy doesn't ride
 * along on the dirty invalidation (and vice versa).
 */
export function watchWorktreeDir(
  wtPath: string,
  onChange: (area: WorktreeDirArea) => void,
): () => void {
  const debouncers: Record<WorktreeDirArea, Debounced> = {
    tree: makeDebounced(() => onChange("tree"), WT_DEBOUNCE_MS),
    sst: makeDebounced(() => onChange("sst"), WT_DEBOUNCE_MS),
  };
  const cancelAll = (): void => {
    debouncers.tree.cancel();
    debouncers.sst.cancel();
  };
  let watcher: FSWatcher | null = null;
  try {
    watcher = watch(
      wtPath,
      { persistent: false, recursive: true },
      (_event, filename) => {
        // `filename` is relative to wtPath; can be null on some macOS
        // event types (treat as "unknown, fire both" so we don't miss
        // anything observable).
        if (filename == null) {
          debouncers.tree.trigger();
          debouncers.sst.trigger();
          return;
        }
        if (filename === ".git" || filename.startsWith(".git/")) return;
        if (filename === ".sst" || filename.startsWith(".sst/")) {
          debouncers.sst.trigger();
          return;
        }
        debouncers.tree.trigger();
      },
    );
    watcher.on("error", (err) => {
      log.warn("worktree watcher error", { err: String(err), wtPath });
    });
  } catch (err) {
    log.warn("worktree watcher failed", { err: String(err), wtPath });
    return cancelAll;
  }
  return () => {
    cancelAll();
    closeSilent(watcher);
  };
}

/**
 * Subscribe to worktree membership changes: `<main>/.git/worktrees/`
 * gains a subdir on `git worktree add` and loses it on remove/prune.
 * This is the push signal for the worktree LIST (external `wt new`,
 * `/split` materializing slices, the detached destroy finishing) —
 * per-worktree admin churn (index.lock, HEAD) is deliberately not
 * watched (non-recursive), it's noise for this purpose.
 *
 * The dir may not exist yet in a repo with zero worktrees; it's
 * created (harmlessly — git treats it as its own) so the watch can
 * always be installed.
 */
export function watchWorktreesAdmin(
  mainClonePath: string,
  onChange: () => void,
): () => void {
  const dir = join(mainClonePath, ".git", "worktrees");
  const debounced = makeDebounced(onChange, REFS_DEBOUNCE_MS);
  let watcher: FSWatcher | null = null;
  try {
    mkdirSync(dir, { recursive: true });
    watcher = watch(dir, { persistent: false }, () => debounced.trigger());
    watcher.on("error", (err) => {
      log.warn("worktrees-admin watcher error", { err: String(err), dir });
    });
  } catch (err) {
    log.warn("worktrees-admin watcher failed", { err: String(err), dir });
    return () => debounced.cancel();
  }
  return () => {
    debounced.cancel();
    closeSilent(watcher);
  };
}

/** Which cross-process state file changed. */
export type WtStateFile = "state" | "archive";

/**
 * Subscribe to writes of the cross-process state files (`state.json`,
 * `archive.json` under `~/.cache/wt/`). Catches mutations from OUTSIDE
 * this process — CLI stack ops run in a Claude session, `wt base set`
 * from a shell, the detached destroy's slug-state reap — so section /
 * stack / archive changes surface without `r`. The TUI's own writes
 * also fire here; that's a harmless duplicate of the explicit
 * invalidation those paths already do (debounced, and the refetch is a
 * cheap file read).
 *
 * Watches the DIRECTORY because both writers rename `<file>.<pid>.tmp`
 * into place and a single-file watch breaks across atomic replaces on
 * macOS; prefix-matching the filename covers the temp and final names
 * (same trick as the github-events marker watcher). The dir also hosts
 * high-churn neighbors (cache.sqlite + WAL); the prefix filter keeps
 * those from over-firing.
 */
export function watchWtStateFiles(
  onChange: (file: WtStateFile) => void,
): () => void {
  const debouncers: Record<WtStateFile, Debounced> = {
    state: makeDebounced(() => onChange("state"), REFS_DEBOUNCE_MS),
    archive: makeDebounced(() => onChange("archive"), REFS_DEBOUNCE_MS),
  };
  const cancelAll = (): void => {
    debouncers.state.cancel();
    debouncers.archive.cancel();
  };
  let watcher: FSWatcher | null = null;
  try {
    mkdirSync(WT_STATE_DIR, { recursive: true });
    watcher = watch(WT_STATE_DIR, { persistent: false }, (_event, filename) => {
      if (filename == null) {
        // Unknown target — fire both rather than miss a write.
        debouncers.state.trigger();
        debouncers.archive.trigger();
        return;
      }
      if (filename.startsWith("state.json")) debouncers.state.trigger();
      else if (filename.startsWith("archive.json")) debouncers.archive.trigger();
    });
    watcher.on("error", (err) => {
      log.warn("wt-state watcher error", { err: String(err), dir: WT_STATE_DIR });
    });
  } catch (err) {
    log.warn("wt-state watcher failed", { err: String(err), dir: WT_STATE_DIR });
    return cancelAll;
  }
  return () => {
    cancelAll();
    closeSilent(watcher);
  };
}

/**
 * Subscribe to per-slug lock churn under `<lockDir>/`. `tryAcquireLock`
 * writes `<slug>.lock` on acquire and every `phase()` update, and
 * unlinks it on release — so this is the push signal for the busy
 * state in BOTH directions: a lock appearing (external destroy, stack
 * replay, `wt new` in a shell) and a lock releasing (setup done). The
 * lock query's while-held poll stays as a backstop, but it only helps
 * once it has already seen the lock; this watcher is what makes the
 * acquire visible in the first place, and what flips "busy: pnpm
 * install" to done the moment the create finishes instead of on the
 * next poll tick.
 *
 * Internal mutex locks (`__fetch_origin__`, `__stack__`, `__wtstate__`,
 * …) share the directory under the `__name__` convention and are
 * filtered out — they're process plumbing, not worktree state. The
 * `withFileLock` mutexes also never rewrite or unlink their files, so
 * they generate no events after first creation anyway.
 *
 * `slug === "*"` means the event carried no filename (rare on macOS);
 * the caller should invalidate coarsely rather than miss a release.
 */
export function watchLockDir(
  lockDir: string,
  onChange: (slug: string) => void,
): () => void {
  const debouncers = new Map<string, Debounced>();
  let disposed = false;
  const debouncerFor = (slug: string): Debounced => {
    let d = debouncers.get(slug);
    if (!d) {
      d = makeDebounced(() => onChange(slug), REFS_DEBOUNCE_MS);
      debouncers.set(slug, d);
    }
    return d;
  };
  const cancelAll = (): void => {
    disposed = true;
    for (const d of debouncers.values()) d.cancel();
    debouncers.clear();
  };
  let watcher: FSWatcher | null = null;
  try {
    mkdirSync(lockDir, { recursive: true });
    watcher = watch(lockDir, { persistent: false }, (_event, filename) => {
      if (disposed) return;
      if (filename == null) {
        debouncerFor("*").trigger();
        return;
      }
      if (!filename.endsWith(".lock")) return;
      const slug = filename.slice(0, -".lock".length);
      if (slug.startsWith("__")) return;
      debouncerFor(slug).trigger();
    });
    watcher.on("error", (err) => {
      log.warn("lock watcher error", { err: String(err), lockDir });
    });
  } catch (err) {
    log.warn("lock watcher failed", { err: String(err), lockDir });
    return cancelAll;
  }
  return () => {
    cancelAll();
    closeSilent(watcher);
  };
}

export type WatchTarget = { slug: string; path: string };

/**
 * Per-slug worktree dir watcher set. Reconcile against the current
 * worktree list each time it changes: add a watcher for any new slug,
 * drop the watcher for any vanished slug. Untouched slugs keep their
 * existing watcher so the FSEvents subscriptions don't churn on every
 * cache update.
 */
export class WorktreeWatchSet {
  private readonly handles = new Map<string, () => void>();
  private readonly onSlugChange: (slug: string, area: WorktreeDirArea) => void;

  constructor(onSlugChange: (slug: string, area: WorktreeDirArea) => void) {
    this.onSlugChange = onSlugChange;
  }

  reconcile(targets: ReadonlyArray<WatchTarget>): void {
    const want = new Map<string, string>();
    for (const t of targets) want.set(t.slug, t.path);
    for (const slug of [...this.handles.keys()]) {
      if (!want.has(slug)) {
        this.handles.get(slug)?.();
        this.handles.delete(slug);
      }
    }
    for (const [slug, path] of want) {
      if (this.handles.has(slug)) continue;
      const dispose = watchWorktreeDir(path, (area) =>
        this.onSlugChange(slug, area),
      );
      this.handles.set(slug, dispose);
    }
  }

  dispose(): void {
    for (const dispose of this.handles.values()) dispose();
    this.handles.clear();
  }
}
