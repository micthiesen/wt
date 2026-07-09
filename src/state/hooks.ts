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
 *    shell action). Skip both when a push trigger or poll already
 *    covers it (tmux lifecycle: explicit invalidations + registry
 *    watcher + 5s backstop; lock state polls every 2s while held).
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
 * 3. Custom actions declare what they affect AND what they require.
 *
 *    `[[actions]]` entries in `config.toml` carry two tag arrays:
 *
 *    - `affects` (`"git"`, `"github"`) — state domains the action
 *      mutates. The TUI subscribes to action completions, refreshes
 *      origin/main for git-affecting actions, and invalidates the matching
 *      state domains on any terminal status.
 *      Defaults: claude actions push commits → `["git", "github"]`;
 *      shell actions are opaque → `[]` (opt in explicitly, e.g. a
 *      `git checkout` shell action sets `affects = ["git"]`).
 *
 *    - `requires` (`"pr"`, `"pr.ready"`) — preconditions evaluated
 *      synchronously against the row state via
 *      `evaluateActionRequirements` (in `core/actions.ts`). The
 *      picker grays out unavailable entries with the reason as the
 *      dim subtitle; the launcher toasts the reason if a digit /
 *      Enter pick targets a blocked entry. Default: `[]`.
 *
 *    Predicates read row state synchronously, so they cascade off
 *    the optimistic patches in rule (1) for free — marking a PR
 *    ready optimistically flips `isDraft`, and the next picker open
 *    shows `requires = ["pr.ready"]` actions as available before the
 *    server confirms; rollback re-blocks them.
 *
 *    For built-in mutations, the equivalent of `affects` is just
 *    calling the relevant refresh helper at the call site
 *    (`refreshGithub`, `invalidateWorktree(slug)`, …); the
 *    equivalent of `requires` is the inline guards at the keybinding
 *    handler (`if (!row?.pr) { toast(…); return; }`). The action
 *    runner exists to bridge config-defined work to the same
 *    invalidation + gating surface.
 *
 * ────────────────────────────────────────────────────────────────────
 */
import { useMemo } from "react";
import {
  MutationObserver,
  matchQuery,
  useQuery,
  useQueryClient,
  type QueryFilters,
  type UseQueryResult,
} from "@tanstack/react-query";

import {
  archiveSlug as archiveOnDisk,
  toggleArchived as toggleArchivedOnDisk,
} from "../core/archive.ts";
import type { DiffContext } from "../core/diff/index.ts";
import { gitRun, invalidateMainFirstParents } from "../core/git.ts";
import { fetchAuthenticatedLogin } from "../core/github.ts";
import type { PullRequest, Worktree } from "../core/types.ts";
import {
  moveGroupPast as moveGroupPastOnDisk,
  placeSlug as placeSlugOnDisk,
  renameSection as renameSectionOnDisk,
  setSlugBase as setSlugBaseOnDisk,
  setSlugSection as setSlugSectionOnDisk,
  swapOrders as swapOrdersOnDisk,
  toggleSectionFolded as toggleSectionFoldedOnDisk,
  toggleSlugAutomationsPaused as toggleSlugAutomationsPausedOnDisk,
  toggleStackAutomationsPaused as toggleStackAutomationsPausedOnDisk,
} from "../core/wtstate.ts";

import { CACHE_DB } from "./client.ts";
import { qk } from "./keys.ts";
import { clearPersistedCache } from "./persister.ts";
import {
  contributorsQuery,
  fetchOriginNow,
  fetchOriginQuery,
  githubQuery,
  tmuxSessionsQuery,
  worktreesQuery,
  type GithubData,
  type TmuxSessionsData,
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
  // The `["github"]` prefix filter used by `mutate` also matches any
  // future / orphaned cache entries that happen to start with the same
  // prefix but carry a different value shape (e.g. a stale persisted
  // `["github", "reviewRequests"]` blob whose data is
  // `ReviewRequestPr[]`, not `GithubData`). Treat anything without a
  // `prs` object as not-our-entry and return it untouched rather than
  // crashing on `data.prs[branch]`.
  const prs = (data as { prs?: unknown }).prs;
  if (!prs || typeof prs !== "object") return data;
  const pr = (prs as Record<string, PullRequest>)[branch];
  if (!pr) return data;
  return {
    ...data,
    prs: { ...(prs as Record<string, PullRequest>), [branch]: patch(pr) },
  };
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

/**
 * Run a mutation with an optimistic cache patch and reconcile-on-settle.
 * Module-level (takes the QueryClient explicitly) so it's directly
 * testable; the TUI calls it through `useWtActions().mutate`.
 *
 * Pipeline: serialize against same-filter mutations (TanStack's
 * mutation `scope` — same `scope.id` → the MutationCache runs them
 * in submission order), cancel any in-flight refetches against
 * `filter` (so they can't clobber the optimistic state on
 * completion), snapshot every matching cache entry, write the patch
 * (synchronous — the badge flips before the network call lands),
 * await `run`, then invalidate the same filter (active refetch
 * reconciles against server truth).
 *
 * Serialization matters: call B must snapshot the cache AFTER call
 * A settles, or A's rollback would clobber B's state. That's also
 * why the cancel/snapshot/patch live in the `mutationFn` rather
 * than `onMutate` — TanStack fires `onMutate` immediately even for
 * a scope-queued mutation (only the mutationFn waits its turn via
 * `canRun`), which would snapshot A's optimistic state into B.
 *
 * Both success and failure paths invalidate (`onSettled`): on throw,
 * rollback every captured snapshot to its prior value AND invalidate
 * so a network error after server commit (rare but possible) gets
 * reconciled rather than leaving the UI lying indefinitely. The
 * settling invalidate is fire-and-forget so the next keypress isn't
 * gated on a network round-trip; the architecture's "active refetch
 * always" promise is best-effort if the refetch itself fails.
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
 * Clobber guard: `cancelQueries` only cancels refetches in flight at
 * call time — a background refetch that STARTS during the await
 * window (e.g. an action-completion subscriber firing
 * `refreshGithub()` mid-mutation) can resolve with pre-mutation
 * server data and overwrite the patch. While `run` is in flight we
 * subscribe to the query cache and re-apply the patch on top of any
 * matching fetch-driven update (`manual` updates — i.e. our own
 * `setQueryData` — are skipped, which is also what prevents the
 * guard from recursing on itself). The settling invalidate still
 * reconciles against true server state afterwards.
 *
 * `run` must throw on failure; mutations that return `{ ok: false,
 * error }` should be wrapped to throw at the call site.
 */
export async function runOptimisticMutation<TData>(
  qc: import("@tanstack/react-query").QueryClient,
  opts: {
    filter: QueryFilters;
    patch: (prev: TData | undefined) => TData | undefined;
    run: () => Promise<void>;
  },
): Promise<void> {
  const { filter, patch, run } = opts;
  // scope.id is the filter's queryKey serialized — falls back to a
  // sentinel when no queryKey was supplied (no current callers omit
  // it, but `QueryFilters` types it as optional).
  const scopeId = filter.queryKey
    ? JSON.stringify(filter.queryKey)
    : "__nokey__";
  let snapshots: Array<readonly [readonly unknown[], TData | undefined]> = [];
  const observer = new MutationObserver<void, Error, void>(qc, {
    scope: { id: scopeId },
    // No `navigator.onLine` signal in a TUI — never let the retryer
    // pause a mutation waiting for an "online" event that can't come.
    networkMode: "always",
    retry: false,
    mutationFn: async () => {
      await qc.cancelQueries(filter);
      snapshots = qc.getQueriesData<TData>(filter);
      qc.setQueriesData<TData>(filter, patch);
      // Clobber guard (see docstring). `matchQuery` is the same
      // predicate `invalidateQueries` uses, so guard coverage is
      // exactly the entries the patch covered.
      const unsubscribe = qc.getQueryCache().subscribe((event) => {
        if (event.type !== "updated") return;
        if (event.action.type !== "success") return;
        if ((event.action as { manual?: boolean }).manual) return;
        if (!matchQuery(filter, event.query)) return;
        qc.setQueryData<TData>(event.query.queryKey, patch);
      });
      try {
        await run();
      } finally {
        unsubscribe();
      }
    },
    onError: () => {
      for (const [key, value] of snapshots) {
        qc.setQueryData([...key], value);
      }
    },
    onSettled: () => {
      void qc.invalidateQueries(filter);
    },
  });
  await observer.mutate();
}

/** Imperative helpers that wrap the raw QueryClient for common ops. */
export function useWtActions() {
  const qc = useQueryClient();

  /** See `runOptimisticMutation` — this just binds the hook's client. */
  function mutate<TData>(opts: {
    filter: QueryFilters;
    patch: (prev: TData | undefined) => TData | undefined;
    run: () => Promise<void>;
  }): Promise<void> {
    return runOptimisticMutation(qc, opts);
  }

  return {
    mutate,
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
      // github query regardless of the branches suffix. Stack
      // relationships are explicit now (wtState parent overrides), so
      // refreshing them is just a `["wtState"]` invalidation. The
      // review-requests query lives off-prefix (see qk.reviewRequests)
      // and gets its own invalidation here.
      await Promise.all([
        qc.fetchQuery(fetchOriginQuery()),
        qc.invalidateQueries({ queryKey: qk.worktrees() }),
        qc.invalidateQueries({ queryKey: ["github"] }),
        qc.invalidateQueries({ queryKey: qk.reviewRequests() }),
        qc.invalidateQueries({ queryKey: qk.wtState() }),
      ]);
      // The first-parent SHA set is not a TanStack query — it's a
      // module-level promise cache in core/git.ts, already dropped by
      // invalidateMainFirstParents() inside fetchOriginQuery. The
      // per-worktree ["wt"] wave is the expensive part, so start it on
      // the next timer turn instead of keeping the key handler/caller
      // parked behind every row's git/fs probes.
      setTimeout(() => {
        void qc.invalidateQueries({ queryKey: ["wt"] });
      }, 50);
    },
    /**
     * Force an origin refresh even if the marker query is still fresh.
     * Passive triggers use this so webhook/action events can advance local
     * main immediately instead of waiting out fetchOriginQuery's staleTime.
     */
    async refreshOrigin(): Promise<void> {
      await fetchOriginNow();
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
     * Refresh stack relationships and the per-worktree diff queries.
     * Stack shape lives in the wtState `stacks` manifests, so re-reading
     * wtState surfaces a freshly-applied or rebased manifest;
     * invalidating `["wt"]` re-runs the per-base diff / sync queries
     * after a rebase rewrites history under a fixed parent.
     */
    async refreshStack(): Promise<void> {
      await Promise.all([
        qc.invalidateQueries({ queryKey: qk.wtState() }),
        qc.invalidateQueries({ queryKey: ["wt"] }),
      ]);
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
      await Promise.all([
        qc.invalidateQueries({ queryKey: ["github"] }),
        qc.invalidateQueries({ queryKey: qk.reviewRequests() }),
      ]);
    },
    /**
     * Invalidate the tmux-sessions query. Call after entering or
     * detaching from a session so the per-row indicator flips
     * immediately rather than waiting for the polling backstop.
     */
    async refreshTmuxSessions(): Promise<void> {
      await qc.invalidateQueries({ queryKey: tmuxSessionsQuery().queryKey });
    },
    /**
     * Invalidate the harness-sessions discovery for a slug. Call after
     * spawning / killing a harness session so the picker entries pick
     * up the new on-disk state. Hits all harnesses since codex /
     * opencode write to shared stores that could surface new entries.
     */
    async refreshHarnessSessions(slug: string): Promise<void> {
      await qc.invalidateQueries({
        queryKey: ["harnessSessions"],
        predicate: (q) => q.queryKey[2] === slug,
      });
    },
    /**
     * Persist a new primary harness selection and invalidate the
     * cached query so observers pick up the change.
     */
    async setPrimaryHarness(id: import("../core/harness/index.ts").HarnessId): Promise<void> {
      const { writePrimaryHarness } = await import("../core/harness/primary.ts");
      writePrimaryHarness(id);
      await qc.invalidateQueries({ queryKey: qk.primaryHarness() });
    },
    /**
     * Cycle the primary harness to the next registered impl and
     * invalidate the cached query.
     */
    async cyclePrimaryHarness(): Promise<import("../core/harness/index.ts").HarnessId> {
      const { cyclePrimaryHarness } = await import("../core/harness/primary.ts");
      const next = cyclePrimaryHarness();
      await qc.invalidateQueries({ queryKey: qk.primaryHarness() });
      return next;
    },
    /**
     * Invalidate the cached LLM summaries for `slug`. The query key
     * doesn't include the persisted-name list, so adding or removing
     * a named session needs an explicit nudge — without this, a
     * freshly-spawned session opens the picker showing "(no summary
     * yet)" for up to staleTime (~30s).
     */
    async refreshClaudeSummaries(slug: string): Promise<void> {
      await qc.invalidateQueries({ queryKey: qk.claudeSummaries(slug) });
    },
    /**
     * Optimistically remove a single (slug, name) claude entry from
     * the tmux-sessions cache. Used by the kill flow so the picker
     * stops listing the dying session as live the instant `x` is
     * pressed — without waiting for the kill to land or a refetch.
     * `slugsByHarness.claude` is recomputed from the
     * filtered `claude` array. No-op if no cache entry exists.
     */
    optimisticRemoveClaude(slug: string, name: string | null): void {
      const key = tmuxSessionsQuery().queryKey;
      qc.setQueryData<TmuxSessionsData>(key, (prev) => {
        if (!prev) return prev;
        const claude = prev.claude.filter(
          (e) => !(e.slug === slug && e.name === name),
        );
        if (claude.length === prev.claude.length) return prev;
        const claudeSlugs = [...new Set(claude.map((e) => e.slug))];
        return {
          ...prev,
          claude,
          slugsByHarness: { ...prev.slugsByHarness, claude: claudeSlugs },
        };
      });
    },
    /**
     * Force the AI summary call to re-run for one worktree. Returns
     * false when there's no cached diff context yet — the caller
     * decides how to message that (we don't want the gesture to mean
     * "warm up cold").
     *
     * `aiSummary` is hash-keyed; force regen refetches the diff
     * context, then `invalidateQueries` on the AI summary entry for
     * the resulting hash. The active observer refetches the queryFn
     * (calling the AI endpoint), and `placeholderData: keepPreviousData`
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
     * Flip the archived flag for a slug. Optimistically patches the
     * archive set so the row reorders immediately under the cursor;
     * the disk write is sync and the post-settle invalidate just
     * confirms. Awaits the mutate call so `useWorktreeRows` has the
     * new state before the caller's next render — cursor-follow logic
     * relies on this.
     */
    async toggleArchived(slug: string): Promise<{ archived: boolean }> {
      let result: { archived: boolean } | null = null;
      await mutate<readonly string[]>({
        filter: { queryKey: qk.archive() },
        patch: (prev) => {
          const set = new Set(prev ?? []);
          if (set.has(slug)) set.delete(slug);
          else set.add(slug);
          return [...set];
        },
        run: async () => {
          // Disk write is synchronous; wrapped in async so it slots
          // into the mutate pipeline. Errors propagate as throws and
          // trigger the rollback path.
          result = toggleArchivedOnDisk(slug);
        },
      });
      // `result` is set inside `run` which always runs before mutate
      // resolves; the `?? throw` here is just a type-narrowing prop.
      if (!result) throw new Error("toggleArchivedOnDisk did not return");
      return result;
    },
    /**
     * Idempotently mark a slug as archived. Used by remove/clean to
     * move a destroying row into the archived section immediately, so
     * the active list isn't visually cluttered during the tail. Fire-
     * and-forget — the disk write is sync, callers don't need to
     * await before dispatching the destroy.
     */
    archive(slug: string): void {
      void mutate<readonly string[]>({
        filter: { queryKey: qk.archive() },
        patch: (prev) => {
          const set = new Set(prev ?? []);
          set.add(slug);
          return [...set];
        },
        run: async () => {
          archiveOnDisk(slug);
        },
      });
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
     * Record (or clear, with `null`) a worktree's fork base — the same
     * per-slug record `wt new --base` writes. Display/diff only: no
     * rebase happens, a manifest slice's parent still wins. Anchors the
     * fork-point sha at merge-base (best-effort, like `wt base set`).
     * Invalidates wtState (row relationship) AND the slug's `["wt"]`
     * queries — diff context and sync counts are computed against the
     * base, so they must re-run under the new one.
     */
    async setBase(wt: Worktree, branch: string | null): Promise<void> {
      if (branch) {
        const mb = await gitRun(["merge-base", wt.branch, branch], wt.path);
        const sha = mb.exitCode === 0 ? mb.stdout.trim() : "";
        setSlugBaseOnDisk(wt.slug, { branch, sha: sha || undefined });
      } else {
        setSlugBaseOnDisk(wt.slug, null);
      }
      await Promise.all([
        qc.invalidateQueries({ queryKey: qk.wtState() }),
        qc.invalidateQueries({ queryKey: qk.wt(wt.slug).all() }),
      ]);
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
    /**
     * Reorder the group list: place group `key` immediately before/
     * after `pastKey` in `sectionsOrder` (Shift+J/K whole-group moves —
     * stack sections, folded headers). Returns true when the move
     * landed, false on a no-op (missing key / no position change).
     * Skips the invalidate on a no-op so the keypress is truly inert
     * (no spurious re-fetch / re-render churn that could otherwise
     * look like a phantom step to the user).
     */
    async moveGroupPast(
      key: string,
      pastKey: string,
      side: "before" | "after",
    ): Promise<boolean> {
      const moved = moveGroupPastOnDisk(key, pastKey, side);
      if (moved) {
        await qc.invalidateQueries({ queryKey: qk.wtState() });
      }
      return moved;
    },
    /**
     * Toggle the per-worktree automations pause flag (persisted in
     * wtstate; Ctrl+A on a non-stack row). Returns the new paused
     * state. The automations engine reads the flag through the wtState
     * query, so the invalidation is what makes the toggle take effect.
     */
    async toggleAutomationsPaused(slug: string): Promise<boolean> {
      const paused = toggleSlugAutomationsPausedOnDisk(slug);
      await qc.invalidateQueries({ queryKey: qk.wtState() });
      return paused;
    },
    /**
     * Toggle the whole-stack automations pause (Ctrl+A on a stack
     * member or its folded header). Keyed by stackId so slices added
     * or re-split later are covered by the same pause.
     */
    async toggleStackAutomationsPaused(stackId: string): Promise<boolean> {
      const paused = toggleStackAutomationsPausedOnDisk(stackId);
      await qc.invalidateQueries({ queryKey: qk.wtState() });
      return paused;
    },
    /**
     * Fold or unfold a section in the list (persisted). Returns the new
     * folded state. The list re-derives its items from the refreshed
     * `wtState`, collapsing/expanding the section's rows.
     */
    async toggleSectionFold(sectionKey: string): Promise<boolean> {
      const folded = toggleSectionFoldedOnDisk(sectionKey);
      await qc.invalidateQueries({ queryKey: qk.wtState() });
      return folded;
    },
  };
}
