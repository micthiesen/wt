/**
 * Filesystem watchers that turn local git activity into query
 * invalidations — a small "push" floor under the existing pull / poll
 * model. Press-`r` becomes a backstop instead of the primary way to see
 * a fresh commit, push, or working-tree edit.
 *
 * Two watch points:
 *   1. `<main>/.git/refs/` recursive — branch + remote ref churn.
 *      Catches commits (refs/heads/*), fetches + pushes (refs/remotes/*),
 *      branch creates / deletes. Shared across the whole repo, one
 *      subscription.
 *   2. Per-worktree dir recursive — working-tree edits (dirty flips).
 *      `.git/` events are filtered out so ref activity doesn't
 *      double-fire here; refs come through (1).
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
import { watch, type FSWatcher } from "node:fs";
import { join } from "node:path";

import { createLogger } from "./logger.ts";

const log = createLogger("[repo-watch]");

/** Coalesce ref bursts — a `git fetch` writes many remote refs in
 *  quick succession; one debounced invalidation pass per burst is
 *  enough. */
const REFS_DEBOUNCE_MS = 300;

/** Editors, formatters, and build chains burst hard on save. Wider
 *  window than refs so we don't refetch dirty mid-burst. */
const WT_DEBOUNCE_MS = 500;

type Debounced = { trigger: () => void; cancel: () => void };

function makeDebounced(onChange: () => void, ms: number): Debounced {
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

function closeSilent(w: FSWatcher | null): void {
  if (!w) return;
  try {
    w.close();
  } catch {
    // already closed
  }
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
 * Subscribe to working-tree edits in one worktree. Recursive watch on
 * the path; `.git/*` events are filtered out (refs already come through
 * the dedicated refs watcher, and the worktree's `.git` is a file
 * pointer anyway).
 */
export function watchWorktreeDir(
  wtPath: string,
  onChange: () => void,
): () => void {
  const debounced = makeDebounced(onChange, WT_DEBOUNCE_MS);
  let watcher: FSWatcher | null = null;
  try {
    watcher = watch(
      wtPath,
      { persistent: false, recursive: true },
      (_event, filename) => {
        // `filename` is relative to wtPath; can be null on some macOS
        // event types (treat as "unknown, fire" so we don't miss
        // anything observable).
        if (filename != null) {
          if (filename === ".git" || filename.startsWith(".git/")) return;
        }
        debounced.trigger();
      },
    );
    watcher.on("error", (err) => {
      log.warn("worktree watcher error", { err: String(err), wtPath });
    });
  } catch (err) {
    log.warn("worktree watcher failed", { err: String(err), wtPath });
    return () => debounced.cancel();
  }
  return () => {
    debounced.cancel();
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
  private readonly onSlugChange: (slug: string) => void;

  constructor(onSlugChange: (slug: string) => void) {
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
      const dispose = watchWorktreeDir(path, () => this.onSlugChange(slug));
      this.handles.set(slug, dispose);
    }
  }

  dispose(): void {
    for (const dispose of this.handles.values()) dispose();
    this.handles.clear();
  }
}
