/**
 * Query definitions — pure data, no React. Each exported factory
 * returns a `queryOptions(...)` result, which gives strong type
 * inference from queryKey → queryFn return type at the hook site.
 */
import { type QueryClient, queryOptions } from "@tanstack/react-query";

import { summarizeDiff, type AiSummary } from "../core/ai.ts";
import { readArchived } from "../core/archive.ts";
import { readWtState, type WtState } from "../core/wtstate.ts";
import { claudeStatus, type ClaudeStatus } from "../core/claude.ts";
import { branchIsGone, branchIsMerged, firstCommitSubject, invalidateMainFirstParents, mainFirstParentShas } from "../core/git.ts";
import { gitActivity, type GitActivity } from "../core/git-activity.ts";
import { buildDiffContext, type DiffContext } from "../core/diff/index.ts";
import { fetchGithub, fetchRepoContributors } from "../core/github.ts";
import { lockStatus } from "../core/locks.ts";
import type {
  Contributor,
  LockMeta,
  MergeQueueEntry,
  PullRequest,
  Worktree,
} from "../core/types.ts";
import { createLogger } from "../core/logger.ts";
import { isOurStageDeployed } from "../core/stage-safety.ts";
import { pluralize } from "../core/text.ts";
import { fetchOrigin, listWorktrees, syncState, type SyncState, worktreeIsDirty } from "../core/worktree.ts";

import { qk } from "./keys.ts";

const aiLog = createLogger("ai");

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

export const wtStateQuery = () =>
  queryOptions({
    queryKey: qk.wtState(),
    queryFn: async (): Promise<WtState> => readWtState(),
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

/**
 * Repo-wide contributor list. Fetched lazily on first reviewer-picker
 * open; the staleTime is generous because the contributor set drifts
 * slowly. Sits under the `["github"]` prefix so `refreshGithub()`
 * clears it alongside the PR fetch.
 */
export const contributorsQuery = () =>
  queryOptions({
    queryKey: qk.contributors(),
    queryFn: async (): Promise<Contributor[]> => fetchRepoContributors(),
    staleTime: 24 * 60 * 60 * 1000,
    gcTime: Number.POSITIVE_INFINITY,
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
    queryFn: async (): Promise<boolean> => isOurStageDeployed(wt),
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

/**
 * Subject of the oldest commit on the branch — fallback title when
 * there's no PR yet. Cheap (one `git log`); short staleTime.
 */
export const wtFirstCommitQuery = (wt: Pick<Worktree, "slug" | "path">) =>
  queryOptions({
    queryKey: qk.wt(wt.slug).firstCommit(),
    queryFn: async (): Promise<string | null> => firstCommitSubject(wt.path),
    staleTime: STALE.mid,
  });

/**
 * Diff context + content hash for the AI summary. The hash is the
 * stable cache key for `aiSummaryQuery`; the prompt body lives only in
 * memory (not serialised to the cache, since it can be megabytes).
 * Local + fast — silent in normal operation, like the other per-wt
 * git queries.
 */
export const wtDiffContextQuery = (wt: Pick<Worktree, "slug" | "path">) =>
  queryOptions({
    queryKey: qk.wt(wt.slug).diffContext(),
    queryFn: async (): Promise<DiffContext | null> => buildDiffContext(wt.path),
    staleTime: STALE.mid,
  });

/**
 * AI summary value as stored in the slug-keyed cache. The hash rides
 * along inside the value so consumers can detect drift (current diff
 * hash vs. cached summary's hash) and decide when to invalidate.
 */
export type AiSummaryWithHash = AiSummary & { hash: string };

/**
 * AI-generated summary of the diff. Keyed by `slug` so observers keep
 * showing the previous summary while a refetch is in flight (the
 * alternative — hashing the queryKey — blanks the brief during the gap
 * because the new key has no cache entry yet).
 *
 * Cross-diff sharing comes from a content-addressed memo
 * (`qk.aiSummaryMemo(hash)`) the queryFn checks before calling LM
 * Studio: equivalent diffs across rebases / amends / branch renames
 * reuse the prior result without a new round-trip. The memo is written
 * after every successful call and never observed directly; it persists
 * via the standard QueryClient dehydration.
 *
 * Pass `null` for `ctx` when the diff context isn't ready; the caller
 * is expected to pair this with `enabled: !!ctx`, in which case the
 * queryFn never runs and emits its early `null`.
 *
 * Hash-mismatch invalidation lives in the consumer hooks (a small
 * effect comparing `data.hash` to the live `ctx.hash`) rather than
 * here — the queryFn doesn't get re-run by react-query just because
 * its closure changed; it needs an explicit invalidate to pick up a
 * new hash.
 *
 * Activity-pane logging mirrors the GitHub-fetch pattern: one start
 * line and one done/failed line per call. Memo hits log dim so a
 * cross-rebase reuse is visible without being noisy.
 */
export const aiSummaryQuery = (
  qc: QueryClient,
  slug: string,
  ctx: { hash: string; prompt: string } | null,
) =>
  queryOptions({
    queryKey: qk.aiSummary(slug),
    queryFn: async (): Promise<AiSummaryWithHash | null> => {
      if (!ctx) return null;
      const memoed = qc.getQueryData<AiSummary>(qk.aiSummaryMemo(ctx.hash));
      if (memoed) {
        aiLog.event.dim(`reused memoed summary for ${slug}`);
        return { hash: ctx.hash, ...memoed };
      }
      aiLog.event.dim(`calling LM Studio for ${slug} (${pluralize(ctx.prompt.length, "char")})...`);
      const start = Date.now();
      try {
        const out = await summarizeDiff(ctx.prompt);
        aiLog.event.dim(`called LM Studio for ${slug} (${formatDuration(Date.now() - start)})`);
        qc.setQueryData<AiSummary>(qk.aiSummaryMemo(ctx.hash), out);
        return { hash: ctx.hash, ...out };
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        aiLog.event.err(
          `LM Studio failed for ${slug} (${formatDuration(Date.now() - start)}): ${msg}`,
        );
        throw err;
      }
    },
    // Slug identity is stable; refetch is driven exclusively by the
    // hash-mismatch effect in the consumer (or by `refreshAiSummary`).
    staleTime: Number.POSITIVE_INFINITY,
    gcTime: Number.POSITIVE_INFINITY,
  });

function formatDuration(ms: number): string {
  return ms < 1000 ? `${ms}ms` : `${(ms / 1000).toFixed(1)}s`;
}
