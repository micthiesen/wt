/**
 * Query definitions — pure data, no React. Each exported factory
 * returns a `queryOptions(...)` result, which gives strong type
 * inference from queryKey → queryFn return type at the hook site.
 */
import { queryOptions } from "@tanstack/react-query";

import { readArchived } from "../core/archive.ts";
import { claudeStatus, type ClaudeStatus } from "../core/claude.ts";
import { branchIsGone, branchIsMerged, invalidateMainFirstParents, mainFirstParentShas } from "../core/git.ts";
import { gitActivity, type GitActivity } from "../core/git-activity.ts";
import { fetchGithub } from "../core/github.ts";
import { lockStatus } from "../core/locks.ts";
import type { LockMeta, MergeQueueEntry, PullRequest, Worktree } from "../core/types.ts";
import { fetchOrigin, isDeployed, listWorktrees, syncState, type SyncState, worktreeIsDirty } from "../core/worktree.ts";

import { qk } from "./keys.ts";

// ---------- Stale-time policy ----------
// Short for cheap fs-backed queries; longer for network/git-heavy ones.
const STALE = {
  fast: 5_000, // fs checks (dirty, lock, deploy)
  mid: 15_000, // listWorktrees, branchIsMerged
  slow: 60_000, // PR fetch, fetchOrigin, firstParents
} as const;

// ---------- Root queries ----------

export const worktreesQuery = () =>
  queryOptions({
    queryKey: qk.worktrees(),
    queryFn: async (): Promise<Worktree[]> => listWorktrees(),
    staleTime: STALE.mid,
  });

export const fetchOriginQuery = () =>
  queryOptions({
    queryKey: qk.fetchOrigin(),
    queryFn: async (): Promise<number> => {
      await fetchOrigin();
      invalidateMainFirstParents();
      return Date.now();
    },
    staleTime: STALE.slow,
  });

export const mainFirstParentsQuery = () =>
  queryOptions({
    queryKey: qk.mainFirstParents(),
    queryFn: async (): Promise<string[]> => [...(await mainFirstParentShas())],
    staleTime: 10 * 60 * 1000,
  });

export const archiveQuery = () =>
  queryOptions({
    queryKey: qk.archive(),
    queryFn: async (): Promise<string[]> => [...readArchived()],
    staleTime: STALE.fast,
  });

export type GithubData = {
  prs: Record<string, PullRequest>;
  mergeQueue: Record<string, MergeQueueEntry>;
};

/**
 * Combined PR + merge-queue fetch scoped to exact worktree branches.
 * One aliased `pullRequests(headRefName:)` per branch + the merge
 * queue, all in a single graphql round trip. Bounded by worktree
 * count rather than repo activity — stays fast regardless of how
 * many PRs the repo churns through.
 */
export const githubQuery = (branches: readonly string[]) =>
  queryOptions({
    queryKey: qk.github(branches),
    queryFn: async (): Promise<GithubData> => {
      const { prs, mergeQueue } = await fetchGithub([...branches]);
      return {
        prs: Object.fromEntries(prs),
        mergeQueue: Object.fromEntries(mergeQueue),
      };
    },
    staleTime: STALE.slow,
  });

// ---------- Per-worktree queries ----------

export const wtDirtyQuery = (wt: Pick<Worktree, "slug" | "path">) =>
  queryOptions({
    queryKey: qk.wt(wt.slug).dirty(),
    queryFn: async (): Promise<boolean> => worktreeIsDirty(wt.path),
    staleTime: STALE.fast,
  });

export const wtLockQuery = (wt: Pick<Worktree, "slug">) =>
  queryOptions({
    queryKey: qk.wt(wt.slug).lock(),
    queryFn: async (): Promise<Partial<LockMeta> | null> => lockStatus(wt.slug),
    staleTime: STALE.fast,
    // Poll more aggressively while a lock is held so "busy" phase text
    // updates without pressing `r`.
    refetchInterval: (query) => (query.state.data ? 2_000 : false),
  });

export const wtDeployQuery = (wt: Pick<Worktree, "slug" | "path">) =>
  queryOptions({
    queryKey: qk.wt(wt.slug).deploy(),
    queryFn: async (): Promise<boolean> => isDeployed(wt.path),
    staleTime: STALE.fast,
  });

export const wtMergedQuery = (wt: Pick<Worktree, "slug" | "branch">) =>
  queryOptions({
    queryKey: qk.wt(wt.slug).merged(),
    queryFn: async (): Promise<boolean> =>
      wt.branch ? branchIsMerged(wt.branch) : false,
    staleTime: STALE.mid,
  });

export const wtGoneQuery = (wt: Pick<Worktree, "slug" | "branch">) =>
  queryOptions({
    queryKey: qk.wt(wt.slug).gone(),
    queryFn: async (): Promise<boolean> =>
      wt.branch ? branchIsGone(wt.branch) : false,
    staleTime: STALE.mid,
  });

export const wtSyncQuery = (wt: Pick<Worktree, "slug" | "path">) =>
  queryOptions({
    queryKey: qk.wt(wt.slug).sync(),
    queryFn: async (): Promise<SyncState> => syncState(wt.path),
    staleTime: STALE.mid,
  });

export const wtClaudeQuery = (wt: Pick<Worktree, "slug" | "path">) =>
  queryOptions({
    queryKey: qk.wt(wt.slug).claude(),
    queryFn: async (): Promise<ClaudeStatus> => claudeStatus({ path: wt.path }),
    staleTime: STALE.fast,
    // Working/waiting states age into "stale" without further file
    // writes, so the panel re-derives state from the cached
    // lastEntryMs. A short refetch keeps the count + freshness
    // honest when a CC session writes new turns.
    refetchInterval: 5_000,
  });

export const wtGitActivityQuery = (wt: Pick<Worktree, "slug" | "path" | "branch">) =>
  queryOptions({
    queryKey: qk.wt(wt.slug).gitActivity(),
    queryFn: async (): Promise<GitActivity> =>
      gitActivity({ path: wt.path, branch: wt.branch }),
    staleTime: STALE.mid,
  });
