/**
 * Query definitions — pure data, no React. Each exported factory
 * returns a `queryOptions(...)` result, which gives strong type
 * inference from queryKey → queryFn return type at the hook site.
 */
import { createHash } from "node:crypto";

import { keepPreviousData, queryOptions } from "@tanstack/react-query";

import { summarizeDiff, summarizeStack, type AiSummary } from "../core/ai.ts";
import { readArchived } from "../core/archive.ts";
import { readClaudeUsage, type ClaudeUsage } from "../core/claude-usage.ts";
import { readRegistry, type RegistrySession } from "../core/claude-registry.ts";
import { wtSessionUuid } from "../core/claude.ts";
import { listClaudeNames } from "../core/claude-sessions.ts";
import { readSummariesForSessions, type SessionSummary } from "../core/claude-summaries.ts";
import { config } from "../core/config.ts";
import { readWtState, type WtState } from "../core/wtstate.ts";
import { claudeStatus, type ClaudeStatus } from "../core/claude.ts";
import { branchIsGone, branchIsMerged, firstCommitSubject, invalidateMainFirstParents, mainFirstParentShas } from "../core/git.ts";
import { gitActivity, type GitActivity } from "../core/git-activity.ts";
import type { DiffContext } from "../core/diff/index.ts";
import { buildDiffContextViaPool } from "../core/diff/pool.ts";
import {
  fetchGithub,
  fetchRepoContributors,
  fetchReviewRequests,
  type ReviewRequestPr,
} from "../core/github.ts";
import { lockStatus } from "../core/locks.ts";
import { detectStacks, type StackMap } from "../core/stack.ts";
import {
  getHarness,
  type HarnessId,
  type HarnessSession,
} from "../core/harness/index.ts";
import {
  type ClaudeSessionEntry,
  listSessions as listTmuxSessions,
} from "../core/tmux.ts";

export type { ClaudeSessionEntry };
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

/**
 * Default `placeholderData` for every query whose queryKey embeds a
 * runtime parameter (branch list, base ref, PR-number list, …). When
 * the parameter shifts (worktree added/removed/renamed, stack parent
 * flips, PR set churns), the observer switches to a different cache
 * entry; without this, the new entry's `data` is `undefined` until
 * the fetch lands and every dependent badge / row blanks together.
 * `keepPreviousData` keeps the prior entry's value on screen across
 * the flip so the UI stays painted. Stable-key queries don't need
 * this — TanStack already retains data across refetches when the
 * key doesn't change. `aiSummaryQuery` opts in at the consumer
 * instead (see its docstring for the cross-slug hazard).
 */
const KEEP_PREV = { placeholderData: keepPreviousData } as const;

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

export type TmuxSessionsData = {
  /**
   * Every live claude session, including primary and named. Multiple
   * entries can share a slug. Drives the sessions picker; consumers
   * that just want "any live claude" should use `slugsByHarness.claude`.
   */
  claude: ClaudeSessionEntry[];
  /**
   * Live-session slug lists keyed by harness id. `claude` is the
   * unique-slug projection of the `claude` entry list (a worktree can
   * host several named claude sessions); `codex`/`opencode` are the
   * single-slot slugs. One uniform `Record<HarnessId, string[]>` so
   * consumers index by harness id instead of branching on it. Arrays
   * (not Sets) because this query is persisted.
   */
  slugsByHarness: Record<HarnessId, string[]>;
  /** Slugs with a live diff session. */
  diff: string[];
  /** Slugs with a live shell session. */
  shell: string[];
  /** Slugs with a live action session (wt-managed wrapper). */
  action: string[];
  /**
   * Raw set of every live tmux session name on the wt-private server.
   * Consumers that need to know whether a specific harness's tmux name
   * is live (e.g. `useHarnessSessions`) read this rather than running
   * a second `list-sessions`. Stored as an array for serialisation;
   * convert to a Set in the consumer hook if needed.
   */
  all: string[];
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
      const { claude, claudeSlugs, codex, opencode, diff, shell, action, all } =
        await listTmuxSessions();
      return {
        claude,
        slugsByHarness: {
          claude: [...claudeSlugs],
          codex: [...codex],
          opencode: [...opencode],
        },
        diff: [...diff],
        shell: [...shell],
        action: [...action],
        all: [...all],
      };
    },
    staleTime: STALE.fast,
    refetchInterval: 2_000,
  });

/**
 * Per-(slug, harness) session discovery. Each impl returns whatever it
 * can derive from its own state stores; this query caches it so the
 * picker / row don't pay the cost on every render. Liveness is NOT
 * baked into the cached value — the consumer hook reannotates against
 * the live tmux name set so a tmux flip doesn't invalidate the
 * discovery cache.
 *
 * `enabled` short-circuits to false when wtPath is empty (defensive —
 * the row pipeline can briefly show empty paths during reordering).
 */
export const harnessSessionsQuery = (
  harnessId: HarnessId,
  slug: string,
  wtPath: string,
) =>
  queryOptions({
    queryKey: qk.harnessSessions(harnessId, slug),
    queryFn: async (): Promise<HarnessSession[]> => {
      const harness = getHarness(harnessId);
      return harness.discoverSessions({ slug, wtPath });
    },
    staleTime: STALE.fast,
    // Claude session state is kept fresh by `watchRegistry` invalidation
    // (its status lives in the fs-watched registry). Codex/OpenCode bake
    // their state into discovery and have no such watcher, so a working
    // session would otherwise show stale state until spawn/kill/refresh —
    // poll while at least one session exists (no empty-dir re-scans).
    refetchInterval: (query) =>
      harnessId === "claude"
        ? false
        : (query.state.data?.length ?? 0) > 0
          ? 3_000
          : false,
    enabled: wtPath !== "",
  });

/**
 * Persisted primary harness id. Read once on mount; mutate via
 * `usePrimaryHarness().setPrimary(id)`. Tiny query, refreshed only on
 * explicit invalidation.
 */
export const primaryHarnessQuery = () =>
  queryOptions({
    queryKey: qk.primaryHarness(),
    queryFn: async () => {
      const { readPrimaryHarness } = await import("../core/harness/primary.ts");
      return readPrimaryHarness();
    },
    staleTime: Infinity,
  });

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
      const { prs, mergeQueue } = await fetchGithub([...branches], signal);
      return {
        prs: Object.fromEntries(prs),
        mergeQueue: Object.fromEntries(mergeQueue),
      };
    },
    staleTime: STALE.slow,
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

export type ClaudeRegistryData = {
  /** Every live claude session on the machine, in readdir order. */
  sessions: readonly RegistrySession[];
  /** Indexed by deterministic UUID for wt-managed session lookups. */
  bySessionId: Readonly<Record<string, RegistrySession>>;
};

/**
 * Live registry of running claude processes. The source file is
 * `~/.claude/sessions/<pid>.json`, rewritten by claude on every status
 * transition + a slow heartbeat. fs.watch in the TUI runtime invalidates
 * this query on file events for near-instant updates; the polling
 * backstop catches anything FSEvents coalesces away and bounds staleness
 * when the watcher isn't installed (CLI mode, watch setup failure).
 */
export const claudeRegistryQuery = () =>
  queryOptions({
    queryKey: qk.claudeRegistry(),
    queryFn: async (): Promise<ClaudeRegistryData> => {
      const sessions = readRegistry();
      const bySessionId: Record<string, RegistrySession> = {};
      for (const s of sessions) bySessionId[s.sessionId] = s;
      return { sessions, bySessionId };
    },
    staleTime: 1_000,
    refetchInterval: 5_000,
  });

/**
 * Per-worktree session summaries — only fetched when the picker
 * actually opens, gated by `enabled` at the call site. Derives the
 * sessionId set internally from `listClaudeNames(slug) + primary`,
 * keeping the query key stable across name churn. The jsonl reads
 * are cached internally by (mtime, size) so repeated opens within an
 * unchanged file are near-free; staleTime lets observers share the
 * same fetch when the picker reopens shortly after closing.
 */
export const claudeSummariesQuery = (wt: Pick<Worktree, "slug" | "path">) =>
  queryOptions({
    queryKey: qk.claudeSummaries(wt.slug),
    queryFn: async (): Promise<Record<string, SessionSummary | null>> => {
      const names: ReadonlyArray<string | null> = [null, ...listClaudeNames(wt.slug)];
      const ids = names.map((n) => wtSessionUuid(wt.path, n));
      return readSummariesForSessions(wt.path, ids);
    },
    staleTime: 30_000,
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
    ...KEEP_PREV,
  });
};

export const wtClaudeQuery = (wt: Pick<Worktree, "slug" | "path">) =>
  queryOptions({
    queryKey: qk.wt(wt.slug).claude(),
    queryFn: async (): Promise<ClaudeStatus> =>
      claudeStatus({ slug: wt.slug, path: wt.path }),
    staleTime: STALE.fast,
    // The session jsonls update every time claude writes a turn or
    // tool-call boundary; polling on a short loop keeps the per-
    // session age + queue counts honest. State (working/waiting/
    // abandoned/idle) is derived in the row via
    // `useClaudeSessionsForSlug`, which subscribes to
    // `tmuxSessionsQuery` (its own poll loop). A tmux state change
    // re-renders the row without rerunning this query.
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
    ...KEEP_PREV,
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
      buildDiffContextViaPool(wt.path, base, signal),
    staleTime: STALE.mid,
    ...KEEP_PREV,
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
    ...KEEP_PREV,
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

export type StackMember = { branch: string; brief: string };

/**
 * Stable signature for a stack: a hash over the sorted *branch names*
 * only. Briefs are passed to the LLM as flavor but deliberately don't
 * participate in the cache key, so:
 *
 *   - Cold start (briefs not loaded yet) → signature stable; restored
 *     persisted title appears immediately without a wasted refetch.
 *   - A member's commits change (brief regenerates) → signature
 *     unchanged; title sticks. Stack themes rarely pivot per-commit,
 *     so this is the right default. A manual regen knob lives below
 *     for the "title is wrong, redo it" case.
 *   - Member set changes (branch added / removed from the chain) →
 *     signature flips → fresh title fetched.
 *
 * Sentinel `__empty__` for an empty list pairs with the `enabled`
 * guard so the queryFn never runs against it.
 */
export function buildStackSignature(
  members: ReadonlyArray<StackMember>,
): string {
  if (members.length === 0) return "__empty__";
  const branches = members.map((m) => m.branch).sort();
  return createHash("sha256").update(branches.join("\0")).digest("hex").slice(0, 16);
}

/**
 * AI-named stack section title. Hash-keyed on the member-branch
 * signature (see `buildStackSignature` for why briefs are excluded
 * from the key) so two stacks with the same membership share one
 * cache entry. Member additions / removals cut a fresh entry; commit
 * churn within members does not.
 *
 * `sectionName` is passed through for the activity log line only; it
 * doesn't participate in the cache key.
 *
 * Persisted: falls into the persister's default-true branch (key
 * length < 3) so the entry survives TUI restarts; restored entries
 * skip the queryFn on first observe, no LM call needed until the
 * member set changes.
 */
export const stackTitleQuery = (
  sectionName: string,
  members: ReadonlyArray<StackMember>,
) =>
  queryOptions({
    queryKey: qk.stackTitle(buildStackSignature(members)),
    queryFn: async ({ signal }): Promise<string> => {
      if (members.length === 0) {
        throw new Error("stackTitleQuery: members empty (enabled guard missed)");
      }
      aiLog.event.dim(`naming stack ${sectionName} (${members.length} members)...`);
      const start = Date.now();
      try {
        const title = await summarizeStack(members, signal);
        aiLog.event.dim(
          `named stack ${sectionName} → "${title}" (${formatDuration(Date.now() - start)})`,
        );
        return title;
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        aiLog.event.err(
          `naming stack ${sectionName} failed (${formatDuration(Date.now() - start)}): ${msg}`,
        );
        throw err;
      }
    },
    enabled: members.length > 0 && !!config.ai,
    staleTime: Number.POSITIVE_INFINITY,
    gcTime: Number.POSITIVE_INFINITY,
  });
