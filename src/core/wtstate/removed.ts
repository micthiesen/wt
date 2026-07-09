import { readWtState, withWtStateLock, writeWtState } from "./io.ts";
import type { RemovedWorktree } from "./types.ts";

/** Bounds on the removed-worktrees history, enforced at write time. */
const REMOVED_MAX_ENTRIES = 30;
const REMOVED_MAX_AGE_MS = 14 * 24 * 60 * 60 * 1000;

/**
 * Record destroyed worktrees into the removed history. Upserts by slug:
 * defined fields of the incoming entry win, existing rich fields (title,
 * PR snapshot) survive a later minimal write — so the TUI's dispatch-time
 * snapshot and `removeWorktree`'s on-success confirmation compose in
 * either order. Prunes by age and caps the list, newest first.
 */
export function recordRemovedWorktrees(
  entries: readonly RemovedWorktree[],
): void {
  if (entries.length === 0) return;
  withWtStateLock(() => {
    const state = readWtState();
    const bySlug = new Map(state.removed.map((e) => [e.slug, e]));
    for (const e of entries) {
      const prev = bySlug.get(e.slug);
      bySlug.set(e.slug, {
        ...prev,
        slug: e.slug,
        branch: e.branch,
        removedAt: e.removedAt,
        ...(e.title !== undefined ? { title: e.title } : {}),
        ...(e.prNumber !== undefined ? { prNumber: e.prNumber } : {}),
        ...(e.prUrl !== undefined ? { prUrl: e.prUrl } : {}),
        ...(e.prState !== undefined ? { prState: e.prState } : {}),
      });
    }
    const cutoff = Date.now() - REMOVED_MAX_AGE_MS;
    const removed = [...bySlug.values()]
      .filter((e) => (Date.parse(e.removedAt) || 0) >= cutoff)
      .sort((a, b) => b.removedAt.localeCompare(a.removedAt))
      .slice(0, REMOVED_MAX_ENTRIES);
    writeWtState({ ...state, removed });
  });
}

/**
 * Drop a slug from the removed history. Called by `createWorktree` so a
 * restored / re-created slug stops appearing as removed. No-op when absent.
 */
export function clearRemovedWorktree(slug: string): void {
  withWtStateLock(() => {
    const state = readWtState();
    if (!state.removed.some((e) => e.slug === slug)) return;
    writeWtState({
      ...state,
      removed: state.removed.filter((e) => e.slug !== slug),
    });
  });
}
