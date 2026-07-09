import { queryOptions } from "@tanstack/react-query";

import { config } from "../../core/config.ts";
import { snapshotForBranches } from "../../core/events/store.ts";
import {
  fetchGithub,
  fetchRepoContributors,
  fetchReviewRequests,
  type ReviewRequestPr,
} from "../../core/github.ts";
import type {
  Contributor,
  MergeQueueEntry,
  PullRequest,
} from "../../core/types.ts";

import { qk } from "../keys.ts";
import { KEEP_PREV, STALE } from "./shared.ts";

export type GithubData = {
  prs: Record<string, PullRequest>;
  /** Merge-queue entries keyed by head branch. */
  mergeQueue: Record<string, MergeQueueEntry>;
};

/**
 * PR fetch (plus merge-queue entries) scoped to exact worktree
 * branches. One aliased `pullRequests(headRefName:)` per branch plus
 * the repo merge queue in a single graphql round trip. Bounded by
 * worktree count rather than repo activity — stays fast regardless of
 * how many PRs the repo churns through.
 */
export const githubQuery = (branches: readonly string[]) =>
  queryOptions({
    queryKey: qk.github(branches),
    queryFn: async ({ signal }): Promise<GithubData> => {
      // Events mode: serve the daemon's warm snapshot when it's fresh and
      // covers these branches, skipping the gh round-trip entirely. The
      // marker watcher (runtime.tsx) drives invalidation; this read is the
      // payoff. Falls back to a live fetch when the snapshot is stale or a
      // worktree was just added (see `snapshotForBranches`).
      if (config.github.events) {
        const snap = snapshotForBranches(branches);
        if (snap) return snap;
      }
      const { prs, mergeQueue } = await fetchGithub([...branches], signal);
      return {
        prs: Object.fromEntries(prs),
        mergeQueue: Object.fromEntries(mergeQueue),
      };
    },
    // With events configured the webhook marker is the freshness driver, so
    // relax the staleTime to a backstop. The `refetchInterval` is a genuine
    // periodic safety net: a dropped fs event (FSEvents coalescing the
    // snapshot + marker writes) must not be able to pin a stale badge until
    // the next unrelated trigger. The interval refetch still serves the warm
    // snapshot when it's fresh, so idle cost is ~one gh fetch per interval.
    // Poll-only setups keep the 60s staleTime and no interval (the refs
    // watcher + manual refresh drive them).
    staleTime: config.github.events?.backstopPollMs ?? STALE.slow,
    refetchInterval: config.github.events ? config.github.events.backstopPollMs : false,
    ...KEEP_PREV,
  });

export type { ReviewRequestPr };

/**
 * PRs that the authenticated user has been asked to review. Single
 * GraphQL `search` call, capped at 50. Lives under the `["github"]`
 * prefix so the `r` refresh and any post-mutation `refreshGithub` both
 * pick it up. Same staleTime as the per-worktree github query — server
 * truth doesn't drift faster on one than the other.
 */
export const reviewRequestsQuery = () =>
  queryOptions({
    queryKey: qk.reviewRequests(),
    queryFn: async ({ signal }): Promise<ReviewRequestPr[]> =>
      fetchReviewRequests(signal),
    // Same freshness model as `githubQuery`: with the webhook daemon
    // configured the marker drives invalidation, so relax staleTime to the
    // backstop and keep a periodic safety net so a dropped fs event or
    // missed delivery can't pin a stale review list (e.g. an approved PR
    // lingering in the section) until the next unrelated trigger. Poll-only
    // setups keep the 60s staleTime and no interval.
    staleTime: config.github.events?.backstopPollMs ?? STALE.slow,
    refetchInterval: config.github.events ? config.github.events.backstopPollMs : false,
    ...KEEP_PREV,
  });

/**
 * Repo-wide contributor list. Fetched lazily on first reviewer-picker
 * open; the staleTime is a week because the contributor set drifts on
 * a scale of weeks (former contractors, new joiners). The picker reads
 * cached data synchronously and triggers a background refetch when
 * stale rather than blocking, so a stale entry just means "next open
 * gets the refreshed list" — not "this open hangs for 6 round-trips".
 * Persisted across runs via the SQLite persister (30-day maxAge).
 */
export const contributorsQuery = () =>
  queryOptions({
    queryKey: qk.contributors(),
    queryFn: async ({ signal }): Promise<Contributor[]> => fetchRepoContributors(signal),
    staleTime: 7 * 24 * 60 * 60 * 1000,
    gcTime: Number.POSITIVE_INFINITY,
  });
