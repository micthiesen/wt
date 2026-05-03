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
import type { DiffContext } from "../core/diff/index.ts";
import { invalidateMainFirstParents } from "../core/git.ts";
import { fetchAuthenticatedLogin } from "../core/github.ts";
import {
  placeSlug as placeSlugOnDisk,
  renameSection as renameSectionOnDisk,
  setSlugSection as setSlugSectionOnDisk,
  swapOrders as swapOrdersOnDisk,
} from "../core/wtstate.ts";

import { CACHE_DB } from "./client.ts";
import { qk } from "./keys.ts";
import { clearPersistedCache } from "./persister.ts";
import {
  contributorsQuery,
  fetchOriginQuery,
  githubQuery,
  worktreesQuery,
  type GithubData,
} from "./queries.ts";
import type { Contributor } from "../core/types.ts";

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
      // Belt-and-suspenders: `qc.clear()` removes cache entries, but
      // active observers sitting on `staleTime: Infinity` (notably
      // the slug-keyed AI summary) don't always re-trigger their
      // queryFn afterwards. Forcing a refetch on every active
      // observer makes "R" deterministic for the AI chain.
      void qc.refetchQueries({ type: "active" });
    },
    /** Invalidate everything for a single worktree (useful after an action). */
    async invalidateWorktree(slug: string): Promise<void> {
      await qc.invalidateQueries({ queryKey: qk.wt(slug).all() });
    },
    /**
     * Fetch the repo-wide contributor list, hitting the cache when
     * available. Used by the reviewer picker so we have a fallback
     * candidate list when GitHub's `suggestedReviewers` is empty.
     */
    async fetchContributors(): Promise<readonly Contributor[]> {
      return await qc.fetchQuery(contributorsQuery());
    },
    /**
     * Currently-authenticated GitHub login (or `null` when gh isn't
     * usable). Process-cached at the source — see
     * `fetchAuthenticatedLogin` in `core/github.ts`.
     */
    async fetchMe(): Promise<string | null> {
      return await fetchAuthenticatedLogin();
    },
    /**
     * Invalidate the combined PR + merge-queue fetch. Use after an
     * action that mutates GitHub state (e.g. enabling auto-merge) so
     * the next render picks up the new server-side state without
     * waiting for the slow staleTime to expire.
     */
    async refreshGithub(): Promise<void> {
      await qc.invalidateQueries({ queryKey: ["github"] });
    },
    /**
     * Force the LM Studio call to re-run for one worktree. Returns
     * false when there's no cached diff context yet — the caller
     * decides how to message that (we don't want the gesture to mean
     * "warm up cold").
     *
     * Sequencing matters: refetch the diff context *first*, then drop
     * the memo entry for the (now possibly-new) hash, then invalidate
     * the slug-keyed query. The naive "fire all three in parallel"
     * shape would let the slug query refetch against its stale
     * closure (old hash) and burn an LM Studio call on the old
     * prompt before the mismatch effect drove a second one for the
     * new diff. Awaiting diffContext first means the slug refetch
     * sees the current hash and we get exactly one LM call.
     */
    async refreshAiSummary(slug: string): Promise<boolean> {
      // The diffContext key is now per-(slug, base) so a worktree can
      // have multiple cached entries (trunk, parent A, parent B…) as
      // its stack relationship evolves. Use prefix-matching to address
      // every cached entry for this slug at once: the row aggregator
      // observes only the *current* base, so on next render the live
      // observer's refetch produces the up-to-date value regardless of
      // which entries we touched here.
      const prefix = ["wt", slug, "diffContext"] as const;
      const existing = qc.getQueriesData<DiffContext | null>({
        queryKey: prefix,
      });
      if (existing.length === 0 || existing.every(([, v]) => !v)) {
        return false;
      }
      await qc.invalidateQueries({ queryKey: prefix });
      const refreshed = qc.getQueriesData<DiffContext | null>({
        queryKey: prefix,
      });
      const fresh = refreshed
        .map(([, v]) => v)
        .find((v): v is DiffContext => v != null);
      if (!fresh) return false;
      qc.removeQueries({ queryKey: qk.aiSummaryMemo(fresh.hash) });
      await qc.invalidateQueries({ queryKey: qk.aiSummary(slug) });
      return true;
    },
    /**
     * Flip the archived flag for a slug. Awaits invalidation so the
     * caller can rely on `useWorktreeRows` having the new state on
     * the next render — required for cursor-follow logic.
     */
    async toggleArchived(slug: string): Promise<{ archived: boolean }> {
      const result = toggleArchivedOnDisk(slug);
      await qc.invalidateQueries({ queryKey: qk.archive() });
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
    /**
     * Assign (or clear, with `null`) a slug's section. Order is reset
     * to the bottom of the target group — the picker convention. Awaits
     * invalidation so cursor-follow can read fresh rows.
     */
    async setSection(slug: string, section: string | null): Promise<void> {
      setSlugSectionOnDisk(slug, section);
      await qc.invalidateQueries({ queryKey: qk.wtState() });
    },
    /**
     * Place a slug at the top or bottom of a section. Used by the
     * unified Shift+J/K cross-section nudge so the moved row lands
     * adjacent to where it was (top of next section, bottom of prev).
     */
    async placeSlug(
      slug: string,
      section: string | null,
      position: "top" | "bottom",
    ): Promise<void> {
      placeSlugOnDisk(slug, section, position);
      await qc.invalidateQueries({ queryKey: qk.wtState() });
    },
    /**
     * Swap two slugs' order values within a single section bucket.
     * `bucketDisplay` must be the bucket's current display order — the
     * write path renormalizes the bucket against this list before
     * swapping, so any unstated entries get materialized cleanly.
     */
    async swapOrder(
      slugA: string,
      slugB: string,
      section: string | null,
      bucketDisplay: readonly string[],
    ): Promise<void> {
      swapOrdersOnDisk(slugA, slugB, section, bucketDisplay);
      await qc.invalidateQueries({ queryKey: qk.wtState() });
    },
    /**
     * Rename a section across every slug that references it. Awaits
     * invalidation so the renamed section and its members are visible
     * in the next render.
     */
    async renameSection(oldName: string, newName: string): Promise<void> {
      renameSectionOnDisk(oldName, newName);
      await qc.invalidateQueries({ queryKey: qk.wtState() });
    },
  };
}
