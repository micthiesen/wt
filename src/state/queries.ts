/**
 * Query definitions — pure data, no React. Each exported factory
 * returns a `queryOptions(...)` result, which gives strong type
 * inference from queryKey → queryFn return type at the hook site.
 */
import { queryOptions } from "@tanstack/react-query";

import { summarizeDiff, type AiSummary } from "../core/ai.ts";
import { readArchived } from "../core/archive.ts";
import { readClaudeUsage, type ClaudeUsage } from "../core/claude-usage.ts";
import { config } from "../core/config.ts";
import { readWtState, type WtState } from "../core/wtstate.ts";
import { claudeStatus, type ClaudeStatus } from "../core/claude.ts";
import { branchIsGone, branchIsMerged, firstCommitSubject, invalidateMainFirstParents, mainFirstParentShas } from "../core/git.ts";
import { gitActivity, type GitActivity } from "../core/git-activity.ts";
import { buildDiffContext, type DiffContext } from "../core/diff/index.ts";
import { fetchGithub, fetchRepoContributors } from "../core/github.ts";
import { lockStatus } from "../core/locks.ts";
import { detectStacks, type StackMap } from "../core/stack.ts";
import { listSessions as listTmuxSessions } from "../core/tmux.ts";
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
import { fetchOrigin, listWorktrees, syncState, type SyncState, worktreeDirtyFiles } from "../core/worktree.ts";

import { qk } from "./keys.ts";

const aiLog = createLogger("ai");

/**
 * Sentinel hash for `aiSummaryQuery` when the caller hasn't built a
 * diff context yet. The query is gated by `enabled: !!ctx` so the
 * queryFn never runs against this key — but the queryKey still has to
 * be a string. A named constant makes the intent obvious vs. a magic
 * `"__noctx__"` literal.
 */
const NO_CTX_HASH = "__noctx__";

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

/**
 * One live claude session as seen by tmux. `name = null` is the
 * primary; a string is a user-named additional session.
 */
export type ClaudeSessionEntry = { slug: string; name: string | null };

export type TmuxSessionsData = {
  /**
   * Every live claude session, including primary and named. Multiple
   * entries can share a slug. Drives the sessions picker; consumers
   * that just want "any live claude" should use `claudeSlugs`.
   */
  claude: ClaudeSessionEntry[];
  /** Slug set with at least one live claude session (primary or named). */
  claudeSlugs: string[];
  /** Slugs with a live diff session. */
  diff: string[];
  /** Slugs with a live shell session. */
  shell: string[];
  /** Slugs with a live action session (wt-managed wrapper). */
  action: string[];
};

/**
 * Slugs with live wt-private tmux sessions, partitioned by kind. One
 * CLI shell-out per refresh covers every worktree and both kinds at
 * once — far cheaper than per-row `has-session` polling or two
 * parallel queries. The 2s refetch keeps both indicators in sync;
 * explicit invalidation still fires on enter/detach so the badges
 * flip immediately rather than waiting up to 2s.
 */
export const tmuxSessionsQuery = () =>
  queryOptions({
    queryKey: qk.tmuxSessions(),
    queryFn: async (): Promise<TmuxSessionsData> => {
      const { claude, claudeSlugs, diff, shell, action } = await listTmuxSessions();
      return {
        claude,
        claudeSlugs: [...claudeSlugs],
        diff: [...diff],
        shell: [...shell],
        action: [...action],
      };
    },
    staleTime: STALE.fast,
    refetchInterval: 2_000,
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
    queryFn: async ({ signal }): Promise<GithubData> => {
      const { prs, mergeQueue } = await fetchGithub([...branches], signal);
      return {
        prs: Object.fromEntries(prs),
        mergeQueue: Object.fromEntries(mergeQueue),
      };
    },
    staleTime: STALE.slow,
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
/**
 * Anthropic API utilization read from the Claude Code statusline's
 * cache file (~/.cache/claude-statusline-usage.json). The statusline
 * is the only thing that hits the API; we just observe its cache, so
 * there's no auth or rate-limit concern here. Refetch every minute so
 * the title bar trails the cache by at most ~60s.
 */
export const claudeUsageQuery = () =>
  queryOptions({
    queryKey: qk.claudeUsage(),
    queryFn: async (): Promise<ClaudeUsage | null> => readClaudeUsage(),
    staleTime: 30_000,
    refetchInterval: 60_000,
  });

export const contributorsQuery = () =>
  queryOptions({
    queryKey: qk.contributors(),
    queryFn: async ({ signal }): Promise<Contributor[]> => fetchRepoContributors(signal),
    staleTime: 7 * 24 * 60 * 60 * 1000,
    gcTime: Number.POSITIVE_INFINITY,
  });

// ---------- Per-worktree queries ----------

export const wtDirtyQuery = (wt: Pick<Worktree, "slug" | "path">) =>
  queryOptions({
    queryKey: qk.wt(wt.slug).dirty(),
    queryFn: async (): Promise<readonly string[]> => worktreeDirtyFiles(wt.path),
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

export const wtSyncQuery = (
  wt: Pick<Worktree, "slug" | "path">,
  effectiveBase?: string | null,
) => {
  const base = effectiveBase ?? `origin/${config.branch.base}`;
  return queryOptions({
    queryKey: qk.wt(wt.slug).sync(base),
    queryFn: async (): Promise<SyncState> => syncState(wt.path, base),
    staleTime: STALE.mid,
  });
};

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

export const wtGitActivityQuery = (
  wt: Pick<Worktree, "slug" | "path" | "branch">,
  effectiveBase?: string | null,
) => {
  const base = effectiveBase ?? `origin/${config.branch.base}`;
  return queryOptions({
    queryKey: qk.wt(wt.slug).gitActivity(base),
    queryFn: async (): Promise<GitActivity> =>
      gitActivity({ path: wt.path, branch: wt.branch }, base),
    staleTime: STALE.mid,
  });
};

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
 *
 * `effectiveBase` defaults to `origin/<config.branch.base>` (trunk).
 * For stacked worktrees the row aggregator passes the parent's branch
 * instead so the diff reflects only this PR's contribution. The query
 * key includes the base so a base flip triggers a refetch via cache
 * miss rather than relying on invalidation.
 */
export const wtDiffContextQuery = (
  wt: Pick<Worktree, "slug" | "path">,
  effectiveBase?: string | null,
) => {
  const base = effectiveBase ?? `origin/${config.branch.base}`;
  return queryOptions({
    queryKey: qk.wt(wt.slug).diffContext(base),
    queryFn: async ({ signal }): Promise<DiffContext | null> =>
      buildDiffContext(wt.path, base, signal),
    staleTime: STALE.mid,
  });
};

/**
 * Cross-worktree stack detection. Walks commit ancestry once for the
 * full worktree set and returns a map of child-slug → parent {slug,
 * branch}. Single source of truth for the "is this stacked on another
 * worktree" question — the row aggregator combines this with the PR's
 * `baseRefName` (declarative fallback) to produce `stackedOn`.
 *
 * Keyed by the sorted branch list (mirrors the github query) so
 * worktree churn re-triggers detection. SHA drift inside a fixed branch
 * set is picked up by the `STALE.mid` staleTime — short enough that a
 * just-pushed commit re-classifies as a parent within ~15s, long enough
 * to not thrash on every render.
 */
export const stackQuery = (worktrees: readonly Worktree[]) => {
  const branches = worktrees
    .filter((w) => !w.isMain && w.branch)
    .map((w) => w.branch);
  return queryOptions({
    queryKey: qk.stack(branches),
    queryFn: async (): Promise<StackMap> => detectStacks(worktrees),
    staleTime: STALE.mid,
  });
};

/**
 * AI-generated summary of the diff, keyed by the diff's content hash.
 * Equivalent diffs across rebases / amends / branch renames hit the
 * same cache entry — that's the whole point of content-addressed
 * keying.
 *
 * The "keep the previous summary visible while a new hash is loading"
 * behavior is the consumer's job: pair this with
 * `placeholderData: keepPreviousData` so a hash flip (diff changed)
 * doesn't blank the description during the gap.
 *
 * Cross-slug hazard: in a `useQuery` consumer (single observer that
 * survives subject changes), `keepPreviousData` will leak the prior
 * slug's summary into the new slug whenever the new slug's queryKey
 * has no cache entry — including the `__noctx__` empty-branch case,
 * which is `enabled: false` so nothing ever overwrites the placeholder.
 * Scope the observer to one slug (e.g. `key={slug}` on the consuming
 * component) so it remounts on slug change. `useQueries` consumers are
 * safe — `QueriesObserver` matches observers by queryHash, so each
 * hash gets its own observer regardless of array position.
 *
 * Pass `null` for `ctx` when the diff context isn't ready; pair with
 * `enabled: !!ctx` so the queryFn never runs and the early `null`
 * sentinel is just a type accommodation.
 */
export const aiSummaryQuery = (
  slug: string,
  ctx: { hash: string; prompt: string } | null,
) =>
  queryOptions({
    // `slug` doesn't participate in the cache key — that's intentional,
    // it's only here for the activity log line. Two worktrees with
    // identical diffs share an entry; the log shows whichever slug
    // triggered the fetch.
    queryKey: qk.aiSummary(ctx?.hash ?? NO_CTX_HASH),
    queryFn: async ({ signal }): Promise<AiSummary> => {
      // The `enabled: !!ctx` guard at the call site makes this branch
      // unreachable. We throw rather than caching `null` defensively:
      // a `null` entry under `NO_CTX_HASH` with `staleTime: Infinity`
      // would be a forever-stuck "no summary" if this ever fired.
      if (!ctx) {
        throw new Error("aiSummaryQuery: ctx is null (enabled guard missed)");
      }
      aiLog.event.dim(`calling LM Studio for ${slug} (${pluralize(ctx.prompt.length, "char")})...`);
      const start = Date.now();
      try {
        const out = await summarizeDiff(ctx.prompt, signal);
        aiLog.event.dim(`called LM Studio for ${slug} (${formatDuration(Date.now() - start)})`);
        return out;
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        aiLog.event.err(
          `LM Studio failed for ${slug} (${formatDuration(Date.now() - start)}): ${msg}`,
        );
        throw err;
      }
    },
    // Hash-keyed: a new diff produces a new cache entry. No staleness
    // policy needed within an entry — the diff content can't change
    // without producing a different hash.
    staleTime: Number.POSITIVE_INFINITY,
    gcTime: Number.POSITIVE_INFINITY,
  });

function formatDuration(ms: number): string {
  return ms < 1000 ? `${ms}ms` : `${(ms / 1000).toFixed(1)}s`;
}
