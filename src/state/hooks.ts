/**
 * React hooks built on top of `queries.ts`. Keeps TanStack Query wiring
 * out of the TUI code. The per-worktree aggregator lives in
 * `tui/hooks/useWorktreeRows.ts` — that's the only consumer, so this
 * file only needs the imperative actions helper.
 *
 * ───────────────────────── State management ─────────────────────────
 *
 * Three rules govern how mutations interact with cached state. They
 * generalise across both built-in mutations (mark-ready, auto-merge,
 * reviewer edits) and config-driven custom actions.
 *
 * 1. Compose optimistic patch + invalidate. Don't pick one.
 *
 *    `mutate({ filter, patch, run })` codifies the dance: cancel any
 *    in-flight refetches against `filter`, snapshot every matching
 *    cache entry, apply `patch` synchronously (badge flips before the
 *    network round-trip lands), await `run`, invalidate the same
 *    filter (active refetch reconciles against server truth), and
 *    rollback the snapshots on throw. `filter` is a prefix and the
 *    patch fans out across every matching entry, so the patch fn must
 *    be safe against entries that don't contain the target row.
 *
 *    Use it for any mutation whose post-state is a clean function of
 *    inputs — PR draft→ready, auto-merge on/off, reviewer add/remove,
 *    archive toggle, section move. Skip the patch and just invalidate
 *    when the post-state cascades unpredictably (kick CI, free-form
 *    shell action). Skip both when polling already covers it (tmux
 *    session lifecycle ticks every 2s, lock state every 2s while
 *    held).
 *
 *    The interaction with synchronous preconditions is the punchline:
 *    optimistic patches show up in row state immediately, so any
 *    inline guard that reads row state cascades for free. Marking a
 *    PR ready unblocks `openReviewerPicker`'s `!pr.isDraft` gate
 *    before the server confirms; a rollback re-blocks it. No
 *    explicit wiring needed between the two mechanisms.
 *
 * 2. Active refetch always.
 *
 *    `invalidateQueries` with default `refetchType: "active"` actively
 *    refetches the observed query rather than just marking it stale.
 *    In a TUI where the user is staring at the affected row, active
 *    is what you want. "Mark stale" is a web-app pattern for
 *    background tabs; we don't have those.
 *
 * 3. Custom actions declare what they affect.
 *
 *    `[[actions]]` entries in `config.toml` carry an `affects` tag
 *    array (`"git"`, `"github"`). The TUI subscribes to action
 *    completions and invalidates the matching state domains when a
 *    run reaches a terminal status. Defaults: claude actions push
 *    commits, so they default to `["git", "github"]`; shell actions
 *    are opaque, so they default to `[]` and the user opts in (e.g.
 *    a `git checkout` shell action sets `affects = ["git"]`).
 *
 *    For a built-in mutation, the equivalent is just calling the
 *    relevant refresh helper at the call site (`refreshGithub`,
 *    `invalidateWorktree(slug)`, …) — the action runner only exists
 *    to bridge config-defined work to the same invalidation surface.
 *
 * ────────────────────────────────────────────────────────────────────
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
import type { PullRequest } from "../core/types.ts";
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
  tmuxSessionsQuery,
  worktreesQuery,
  type GithubData,
} from "./queries.ts";
import type { Contributor } from "../core/types.ts";

/**
 * In-place patch helper for a single PR inside the github cache. The
 * github query is keyed by the sorted branch list, not by PR number,
 * so callers don't have a single concrete queryKey — they patch every
 * matching `["github", …]` entry via `mutate({ filter: { queryKey:
 * ["github"] }, ... })`. Returns the input unchanged when there's no
 * matching PR (cache miss for this branch); the follow-up invalidate
 * only re-fetches entries with active observers, so a cold cache that
 * nothing observes stays missing until something subscribes.
 */
export function patchPullRequest(
  data: GithubData | undefined,
  branch: string,
  patch: (pr: PullRequest) => PullRequest,
): GithubData | undefined {
  if (!data) return data;
  const pr = data.prs[branch];
  if (!pr) return data;
  return { ...data, prs: { ...data.prs, [branch]: patch(pr) } };
}

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
     * Run a mutation with an optimistic cache patch and reconcile-on-success.
     *
     * Four steps: cancel any in-flight refetches against `filter` (so
     * they can't clobber the optimistic state on completion), snapshot
     * every matching cache entry, write the patch (synchronous — the
     * badge flips before the network call lands), await `run`, then
     * invalidate (active refetch reconciles against server truth). On
     * throw, rollback every captured snapshot to its prior value and
     * rethrow. The post-success invalidate is fire-and-forget; if its
     * refetch fails the optimistic state remains until the next user
     * refresh.
     *
     * `filter` is a queryKey prefix. The patch runs against every
     * matching entry — for queries keyed by inputs the call site
     * doesn't have (e.g. the github query keyed by sorted branch list,
     * not PR number), this is what makes the helper work, but it also
     * means the patch fn must be safe against entries that don't
     * contain the target row (see `patchPullRequest`'s "no PR for
     * branch → return data unchanged" path). Reconcile only happens
     * for entries with active observers; cache entries observed by no
     * one stay patched until eviction.
     *
     * `run` must throw on failure; mutations that return `{ ok: false,
     * error }` should be wrapped to throw at the call site.
     *
     * Concurrent calls against the same filter are not safe — call B
     * snapshots A's optimistic state as its baseline, so an A failure
     * after B has patched will rollback to B's optimistic state, not
     * A's pre-patch state. The TUI fires one mutation per keypress so
     * the constraint is naturally honored; if that ever changes,
     * serialize via TanStack's MutationObserver.
     */
    async mutate<TData>(opts: {
      filter: { queryKey: readonly unknown[] };
      patch: (prev: TData | undefined) => TData | undefined;
      run: () => Promise<void>;
    }): Promise<void> {
      const { filter, patch, run } = opts;
      await qc.cancelQueries(filter);
      const snapshots = qc.getQueriesData<TData>(filter);
      qc.setQueriesData<TData>(filter, patch);
      try {
        await run();
        void qc.invalidateQueries(filter);
      } catch (err) {
        for (const [key, value] of snapshots) qc.setQueryData(key, value);
        throw err;
      }
    },
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
      // the AI summary) don't always re-trigger their queryFn
      // afterwards. Forcing a refetch on every active observer makes
      // "R" deterministic for the AI chain.
      void qc.refetchQueries({ type: "active" });
    },
    /** Invalidate everything for a single worktree (useful after an action). */
    async invalidateWorktree(slug: string): Promise<void> {
      await qc.invalidateQueries({ queryKey: qk.wt(slug).all() });
    },
    /**
     * Read the repo-wide contributor list from cache without blocking
     * on the network when warm. If the cached entry is stale we kick
     * off a background refetch so the *next* picker open sees the
     * refreshed list, but return what we already have right now — a
     * stale list is fine, what's not fine is paying 6 sequential gh
     * round-trips on every open. The one exception is a truly cold
     * cache (first-ever open, or the persister evicted past its
     * 30-day maxAge): there we await one fetch so the picker has
     * *something* to show beyond an empty fallback list.
     */
    async fetchContributors(): Promise<readonly Contributor[]> {
      const opts = contributorsQuery();
      const cached = qc.getQueryData<readonly Contributor[]>(opts.queryKey);
      if (cached === undefined) {
        return await qc.fetchQuery(opts);
      }
      const state = qc.getQueryState(opts.queryKey);
      const isStale =
        !state ||
        Date.now() - state.dataUpdatedAt > (opts.staleTime as number);
      if (isStale) {
        void qc.prefetchQuery(opts);
      }
      return cached;
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
     * Invalidate the tmux-sessions query. Call after entering or
     * detaching from a session so the per-row indicator flips
     * immediately rather than waiting for the 2s polling tick.
     */
    async refreshTmuxSessions(): Promise<void> {
      await qc.invalidateQueries({ queryKey: tmuxSessionsQuery().queryKey });
    },
    /**
     * Force the LM Studio call to re-run for one worktree. Returns
     * false when there's no cached diff context yet — the caller
     * decides how to message that (we don't want the gesture to mean
     * "warm up cold").
     *
     * `aiSummary` is hash-keyed; force regen refetches the diff
     * context, then `invalidateQueries` on the AI summary entry for
     * the resulting hash. The active observer refetches the queryFn
     * (calling LM Studio), and `placeholderData: keepPreviousData`
     * keeps the prior summary on screen during the gap. Using
     * `invalidateQueries` instead of `removeQueries` is deliberate:
     * deleting the entry blanks the display because the observer's
     * keepPreviousData fallback only kicks in on a queryKey change,
     * not on an evicted same-key entry.
     */
    async refreshAiSummary(slug: string): Promise<boolean> {
      // The diffContext key is per-(slug, base) so a worktree can
      // have multiple cached entries (trunk, parent A, parent B…) as
      // its stack relationship evolves. Prefix-match to address every
      // cached entry for this slug; the row aggregator observes only
      // the *current* base, so on next render the live observer's
      // refetch produces the up-to-date value regardless of which
      // entries we touched here.
      const prefix = ["wt", slug, "diffContext"] as const;
      const existing = qc.getQueriesData<DiffContext | null>({
        queryKey: prefix,
      });
      if (existing.length === 0 || existing.every(([, v]) => !v)) {
        return false;
      }
      // `invalidateQueries` awaits the refetch of any active observer
      // (default `refetchType: "active"`), so by the time this resolves
      // the diff context cache holds the new hash.
      await qc.invalidateQueries({ queryKey: prefix });
      const refreshed = qc.getQueriesData<DiffContext | null>({
        queryKey: prefix,
      });
      if (refreshed.every(([, v]) => !v)) return false;
      // Invalidate (don't remove) the AI summary entry for each
      // still-present hash. Invalidate triggers an active-observer
      // refetch even with `staleTime: Infinity`, and the cache entry
      // stays put so `keepPreviousData` has data to show during the
      // gap.
      await Promise.all(
        refreshed
          .filter(([, ctx]) => !!ctx)
          .map(([, ctx]) =>
            qc.invalidateQueries({ queryKey: qk.aiSummary(ctx!.hash) }),
          ),
      );
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
