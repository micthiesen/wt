/**
 * React hooks built on top of `queries.ts`. Keeps TanStack Query wiring
 * out of the TUI code. The per-worktree aggregator lives in
 * `tui/hooks/useWorktreeRows.ts` — that's the only consumer, so this
 * file only needs the imperative actions helper.
 */
import { useMemo } from "react";
import { useQuery, useQueryClient, type UseQueryResult } from "@tanstack/react-query";

import {
  archiveSlug as archiveOnDisk,
  toggleArchived as toggleArchivedOnDisk,
} from "../core/archive.ts";
import { invalidateMainFirstParents } from "../core/git.ts";

import { CACHE_DB } from "./client.ts";
import { qk } from "./keys.ts";
import { clearPersistedCache } from "./persister.ts";
import { fetchOriginQuery, githubQuery, worktreesQuery, type GithubData } from "./queries.ts";

/**
 * Observe the combined GitHub query, scoped to the current set of
 * worktree branches. Derives branches from `worktreesQuery` so both
 * consumers (list-row aggregator + details pane) share one observer
 * and one fetch. Sort stabilizes the queryKey against worktree-list
 * ordering changes.
 */
export function useGithub(): UseQueryResult<GithubData, Error> {
  const wtList = useQuery(worktreesQuery());
  const branches = useMemo(() => {
    return (wtList.data ?? [])
      .filter((w) => !w.isMain && !!w.branch)
      .map((w) => w.branch as string)
      .sort();
  }, [wtList.data]);
  return useQuery(githubQuery(branches));
}

/** Imperative helpers that wrap the raw QueryClient for common ops. */
export function useWtActions() {
  const qc = useQueryClient();
  return {
    /**
     * Refetch only the observed queries that are past their staleTime.
     * Unlike `refreshAll`, this doesn't run `git fetch origin` and
     * doesn't touch queries that are still fresh. Intended for passive
     * triggers (terminal focus, etc.) where we just want the displayed
     * data to pick up recent drift without doing unnecessary work.
     * Returns the count of queries that will be refetched.
     */
    refreshStale(): number {
      const stale = qc
        .getQueryCache()
        .findAll({ stale: true, type: "active" });
      if (stale.length === 0) return 0;
      void qc.refetchQueries({ stale: true, type: "active" });
      return stale.length;
    },
    /**
     * Sync everything against live truth: `git fetch origin`, re-query
     * the worktree list, re-fetch PRs, and invalidate every
     * per-worktree field. This is the everyday "I want fresh data"
     * button — cheap enough to press whenever.
     */
    async refreshAll(): Promise<void> {
      // `queryKey: ["github"]` uses prefix match — invalidates every
      // github query regardless of the branches suffix.
      await Promise.all([
        qc.fetchQuery(fetchOriginQuery()),
        qc.invalidateQueries({ queryKey: qk.worktrees() }),
        qc.invalidateQueries({ queryKey: ["github"] }),
      ]);
      await Promise.all([
        qc.invalidateQueries({ queryKey: qk.mainFirstParents() }),
        qc.invalidateQueries({ queryKey: ["wt"] }),
      ]);
    },
    /**
     * Nuke every cached query — in-memory *and* the SQLite blob on
     * disk — drop the in-process `mainFirstParents` cache, then kick
     * off a `git fetch origin` to seed the refetch. Active observers
     * re-issue their own fetches immediately, so the UI returns to a
     * loading state and rebuilds from scratch.
     */
    async clearAll(): Promise<void> {
      qc.clear();
      clearPersistedCache(CACHE_DB);
      invalidateMainFirstParents();
      // Not observed by any component, so it won't auto-refetch on
      // clear — kick it off explicitly so the first-parents cache gets
      // repopulated alongside the observed queries.
      void qc.fetchQuery(fetchOriginQuery());
    },
    /** Invalidate everything for a single worktree (useful after an action). */
    async invalidateWorktree(slug: string): Promise<void> {
      await qc.invalidateQueries({ queryKey: qk.wt(slug).all() });
    },
    /** Flip the archived flag for a slug and re-query. Returns the new state. */
    toggleArchived(slug: string): { archived: boolean } {
      const result = toggleArchivedOnDisk(slug);
      void qc.invalidateQueries({ queryKey: qk.archive() });
      return result;
    },
    /**
     * Idempotently mark a slug as archived. Used by remove/clean to
     * move a destroying row into the archived section immediately, so
     * the active list isn't visually cluttered during the tail.
     */
    archive(slug: string): void {
      archiveOnDisk(slug);
      void qc.invalidateQueries({ queryKey: qk.archive() });
    },
  };
}
