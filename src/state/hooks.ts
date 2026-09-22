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
import { useEffect, useMemo, useSyncExternalStore } from "react";
import { Duration, Effect, Fiber } from "effect";
import {
  MutationObserver,
  matchQuery,
  replaceEqualDeep,
  useQuery,
  useQueryClient,
  type QueryFilters,
  type UseQueryResult,
} from "@tanstack/react-query";

import {
  archiveSlug as archiveOnDisk,
  reapRemoteArchived,
  toggleArchived as toggleArchivedOnDisk,
} from "../core/archive.ts";
import { config } from "../core/config.ts";
import { issueStatusIds, type IssueStatuses } from "../core/issue-status.ts";
import { issueStatusExpectations } from "./issue-status.ts";
import type { DiffContext } from "../core/diff/index.ts";
import { causeMessage } from "../core/errors.ts";
import { gitRun, invalidateMainFirstParents } from "../core/git.ts";
import { fetchAuthenticatedLogin } from "../core/github.ts";
import { createLogger } from "../core/logger.ts";
import { markSelfSectionWrite } from "./self-writes.ts";
import type { PullRequest, Worktree } from "../core/types.ts";
import type { WorkStatusRecord } from "../core/work-status.ts";
import {
  moveGroupPast as moveGroupPastOnDisk,
  placeSlug as placeSlugOnDisk,
  renameSection as renameSectionOnDisk,
  setSectionFolded as setSectionFoldedOnDisk,
  setSlugBase as setSlugBaseOnDisk,
  setWorktreeSection as setWorktreeSectionOnDisk,
  setSlugIssueId as setSlugIssueIdOnDisk,
  dismissReviewRequest as dismissReviewRequestOnDisk,
  setSlugWorkStatus as setSlugWorkStatusOnDisk,
  swapOrders as swapOrdersOnDisk,
  toggleSectionFolded as toggleSectionFoldedOnDisk,
  toggleRemovedAutomationsPaused as toggleRemovedAutomationsPausedOnDisk,
  toggleSlugAutomationsPaused as toggleSlugAutomationsPausedOnDisk,
  toggleStackAutomationsPaused as toggleStackAutomationsPausedOnDisk,
} from "../core/wtstate.ts";

import { CACHE_DB } from "./client.ts";
import { qk } from "./keys.ts";
import { clearPersistedCache } from "./persister.ts";
import { operationErrors } from "./queries/boundary.ts";
import {
  contributorsQuery,
  fetchOriginQuery,
  githubQuery,
  issueStatusesQuery,
  remoteWorktreesQuery,
  tmuxSessionsQuery,
  worktreesQuery,
  wtStateQuery,
  type GithubData,
  type TmuxSessionsData,
} from "./queries.ts";
import { refreshOrigin as forceFetchOrigin } from "./queries/worktree.ts";
import type { Contributor } from "../core/types.ts";

const io = operationErrors("hooks");
const log = createLogger("[state]");

/**
 * Fork `effect` into the background and log (not throw) if it fails.
 * Used for the "kick off, don't block on it" tail of `refreshAll` /
 * `clearAll` — a bare `Effect.runFork` nested inside an already-
 * running Effect works but hides failures; this composes with
 * `Effect.forkDetach` (survives the parent effect's completion, same
 * as the old `Effect.runFork`) and narrates a failure the same way
 * `useGithub`'s reap-remote-archive catch does.
 */
function forkLogged<A, E>(
  label: string,
  effect: Effect.Effect<A, E>,
): Effect.Effect<void, never> {
  return effect.pipe(
    Effect.catch((cause) =>
      Effect.sync(() => {
        log.warn(`${label} failed`, { err: causeMessage(cause) });
      }),
    ),
    Effect.forkDetach,
    Effect.asVoid,
  );
}

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

/** Only a successful fetch in this mount may reconcile remote archive state. */
export function shouldReapRemoteArchive(result: {
  isSuccess: boolean;
  isFetchedAfterMount: boolean;
}): boolean {
  return result.isSuccess && result.isFetchedAfterMount;
}

/**
 * Observe the combined GitHub query, scoped to the current local + remote
 * fleet branches. Both consumers (list-row aggregator + details pane) share
 * one observer and one fetch. Dedupe handles the same branch appearing in
 * both locations; sort stabilizes the query key against inventory ordering.
 */
export function useGithub(): UseQueryResult<GithubData, Error> {
  const wtList = useQuery(worktreesQuery());
  const remoteList = useQuery(remoteWorktreesQuery());
  useEffect(() => {
    if (!shouldReapRemoteArchive(remoteList) || !config.remote) return;
    Effect.runSync(
      io
        .sync("reap remote archive", () =>
          reapRemoteArchived(
            config.remote!.host,
            new Set((remoteList.data ?? []).map((row) => row.slug)),
          ),
        )
        .pipe(
          Effect.catch((error) =>
            Effect.sync(() =>
              createLogger("[github]").warn("remote archive reconciliation failed", {
                err: error.message,
              }),
            ),
          ),
        ),
    );
  }, [remoteList.data, remoteList.isFetchedAfterMount, remoteList.isSuccess]);
  const branches = useMemo(() => {
    const local = (wtList.data ?? [])
      .filter((w) => !w.isMain && !!w.branch)
      .map((w) => w.branch as string);
    const remote = (remoteList.data ?? [])
      .filter((w) => !!w.branch)
      .map((w) => w.branch);
    return [...new Set([...local, ...remote])].sort();
  }, [wtList.data, remoteList.data]);
  return useQuery(githubQuery(branches));
}

/** Local overrides and remote inventory IDs share one provider-neutral batch. */
export function useIssueStatuses() {
  const qc = useQueryClient();
  const local = useQuery(worktreesQuery());
  const remote = useQuery(remoteWorktreesQuery());
  const state = useQuery(wtStateQuery());
  const ids = useMemo(() => issueStatusIds([
    ...(local.data ?? []).filter((wt) => !wt.isMain).map((wt) => ({
      slug: wt.slug, issueId: state.data?.slugs[wt.slug]?.issueId,
    })),
    // Worker snapshots already resolved the override. Null is asserted none,
    // not permission to resurrect the ID embedded in the remote slug.
    ...(remote.data ?? []).map((wt) => ({ slug: wt.slug, issueId: wt.issueId ?? "" })),
  ], config.issueTracker?.prefix), [local.data, remote.data, state.data?.slugs]);
  const query = useQuery({
    ...issueStatusesQuery(ids),
    // Wait for overrides: slug-only identity is not an adequate interim key.
    enabled: !!config.issueTracker?.statusCommand && ids.length > 0 && state.data !== undefined,
  });
  const store = issueStatusExpectations(qc);
  const expected = useSyncExternalStore(store.subscribe, store.getSnapshot);
  const data = useMemo<IssueStatuses | undefined>(() => {
    if (!expected.size) return query.data;
    const merged = { ...query.data };
    for (const [id, value] of expected) if (ids.includes(id)) merged[id] = value.status;
    return merged;
  }, [query.data, expected, ids]);
  return { ...query, data, expected, confirmedData: query.data, ids };
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
 * server data and overwrite the patch. While the guard is up we
 * subscribe to the query cache and re-apply the patch on top of any
 * matching fetch-driven update (`manual` updates — i.e. our own
 * `setQueryData` — are skipped, which is also what prevents the
 * guard from recursing on itself).
 *
 * **The guard outlives `run`, and it has to.** `run` resolving means
 * GitHub ACCEPTED the mutation, not that GitHub will now serve it: its
 * GraphQL reads lag its own writes by a beat, so the settling
 * invalidate — which fires immediately after — routinely lands
 * pre-mutation data. The badge then flips back to the value the user
 * just changed, and self-corrects a few seconds later on the next
 * fetch. That reads as "it didn't work", which is the opposite of what
 * an optimistic patch is for, and it is worst on exactly the mutations
 * with a webhook behind them, because the extra fetch is another
 * chance to land stale.
 *
 * So the guard runs until one of three things happens: a fetch arrives
 * that the patch no longer CHANGES (the server has caught up — the
 * only real end condition, and it self-terminates so a later genuine
 * change by someone else isn't suppressed), the mutation fails (the
 * patch is rolled back and must not be re-applied), or
 * `SETTLE_GUARD_MS` elapses. The deadline is a backstop, not the
 * mechanism: some patches are never confirmable by the field they
 * patched — arming merge-when-ready on a queue base leaves
 * `autoMergeRequest` null forever and shows up as a queue entry
 * instead — and a guard with no deadline would pin those until the
 * process died.
 *
 * `run` is either an `Effect` (preferred — composes directly, typed
 * failure) or a Promise-returning thunk that must throw/reject on
 * failure (the TUI's flow call sites haven't all migrated off this
 * yet); mutations that return `{ ok: false, error }` should be wrapped
 * to throw/fail at the call site.
 */
/**
 * How long the clobber guard keeps re-applying the patch after `run`
 * resolves. Long enough to cover GitHub's read-after-write lag plus the
 * settling refetch it triggers; short enough that a patch the server
 * will never echo back stops pinning the cache. See the docstring.
 */
const SETTLE_GUARD_MS = 12_000;

/** Apply an archive target state idempotently for the settle guard. */
export function patchArchivedKeys(
  prev: readonly string[] | undefined,
  key: string,
  archived: boolean,
): readonly string[] {
  const set = new Set(prev ?? []);
  if (archived) set.add(key);
  else set.delete(key);
  return [...set];
}

export async function runOptimisticMutation<TData, E = unknown>(
  qc: import("@tanstack/react-query").QueryClient,
  opts: {
    filter: QueryFilters;
    patch: (prev: TData | undefined) => TData | undefined;
    run: (() => Promise<void>) | Effect.Effect<void, E>;
    /** Test seam for the post-settle guard deadline. */
    settleGuardMs?: number;
  },
): Promise<void> {
  const { filter, patch, run, settleGuardMs = SETTLE_GUARD_MS } = opts;
  // scope.id is the filter's queryKey serialized — falls back to a
  // sentinel when no queryKey was supplied (no current callers omit
  // it, but `QueryFilters` types it as optional).
  const scopeId = filter.queryKey
    ? JSON.stringify(filter.queryKey)
    : "__nokey__";
  let snapshots: Array<readonly [readonly unknown[], TData | undefined]> = [];
  let unsubscribe: (() => void) | null = null;
  let guardFiber: Fiber.Fiber<void, never> | null = null;
  let failed = false;
  const stopGuard = (): void => {
    if (guardFiber !== null) {
      const fiber = guardFiber;
      guardFiber = null;
      Effect.runFork(Fiber.interrupt(fiber));
    }
    unsubscribe?.();
    unsubscribe = null;
  };
  const observer = new MutationObserver<void, Error, void>(qc, {
    scope: { id: scopeId },
    // No `navigator.onLine` signal in a TUI — never let the retryer
    // pause a mutation waiting for an "online" event that can't come.
    networkMode: "always",
    retry: false,
    mutationFn: () =>
      Effect.runPromise(
        Effect.gen(function* () {
          yield* Effect.tryPromise({
            try: () => qc.cancelQueries(filter),
            catch: io.wrap("cancel queries"),
          });
          yield* Effect.sync(() => {
            snapshots = qc.getQueriesData<TData>(filter);
            qc.setQueriesData<TData>(filter, patch);
          });
          // Clobber guard (see docstring). `matchQuery` is the same
          // predicate `invalidateQueries` uses, so guard coverage is
          // exactly the entries the patch covered.
          unsubscribe = yield* Effect.sync(() =>
            qc.getQueryCache().subscribe((event) => {
              if (event.type !== "updated") return;
              if (event.action.type !== "success") return;
              if ((event.action as { manual?: boolean }).manual) return;
              if (!matchQuery(filter, event.query)) return;
              const data = event.query.state.data as TData | undefined;
              // The server has caught up the moment a fetch lands that the
              // patch would not change. Structural compare rather than
              // identity: `patch` builds fresh objects every call, so it is
              // never `===` its input even when it changes nothing.
              if (replaceEqualDeep(data, patch(data)) === data) {
                stopGuard();
                return;
              }
              qc.setQueryData<TData>(event.query.queryKey, patch);
            }),
          );
          // `run` is either an Effect — passed straight through, so the
          // caller's typed failure (and its message) is what `mutate`
          // rejects with, exactly as a direct call would — or a legacy
          // Promise thunk adopted at the boundary.
          yield* Effect.isEffect(run)
            ? run
            : Effect.tryPromise({ try: run, catch: io.wrap("mutation") });
        }),
      ),
    onError: () => {
      failed = true;
      // Stop before the rollback, or the guard re-applies the patch on
      // top of the snapshot it just restored.
      stopGuard();
      for (const [key, value] of snapshots) {
        qc.setQueryData([...key], value);
      }
    },
    onSettled: () => {
      // Runs after onError too, so the deadline must not re-arm a guard
      // a failure already took down.
      if (!failed && unsubscribe) {
        guardFiber = Effect.runFork(
          Effect.sleep(Duration.millis(settleGuardMs)).pipe(
            Effect.andThen(
              Effect.sync(() => {
                guardFiber = null;
                stopGuard();
              }),
            ),
          ),
        );
      }
      void qc.invalidateQueries(filter);
    },
  });
  try {
    await observer.mutate();
  } catch (err) {
    // `observer.mutate()` rejects on failure; onError has already
    // stopped the guard, but a throw from anywhere else must not leave
    // a subscription pinned to a patch nobody is tracking.
    stopGuard();
    throw err;
  }
}

/** Imperative helpers that wrap the raw QueryClient for common ops. */
export function useWtActions() {
  const qc = useQueryClient();

  /** See `runOptimisticMutation` — this just binds the hook's client. */
  function mutate<TData, E = unknown>(opts: {
    filter: QueryFilters;
    patch: (prev: TData | undefined) => TData | undefined;
    run: (() => Promise<void>) | Effect.Effect<void, E>;
  }): Promise<void> {
    return runOptimisticMutation(qc, opts);
  }

  const queryClientEffect = <A>(evaluate: () => PromiseLike<A>) =>
    Effect.tryPromise({
      try: evaluate,
      catch: io.wrap("query client operation"),
    });
  const invalidate = (filter: QueryFilters) =>
    queryClientEffect(() => qc.invalidateQueries(filter));
  const writeWtState = <A>(evaluate: () => A): Promise<A> =>
    Effect.runPromise(
      io
        .sync("write wt state", evaluate)
        .pipe(Effect.tap(() => invalidate({ queryKey: qk.wtState() }))),
    );

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
      const stale = qc.getQueryCache().findAll({ stale: true, type: "active" });
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
    refreshAll(): Promise<void> {
      // `queryKey: ["github"]` uses prefix match — invalidates every
      // github query regardless of the branches suffix. Stack
      // relationships are explicit now (wtState parent overrides), so
      // refreshing them is just a `["wtState"]` invalidation. The
      // review-requests query lives off-prefix (see qk.reviewRequests)
      // and gets its own invalidation here.
      return Effect.runPromise(
        Effect.all(
          [
            queryClientEffect(() => qc.fetchQuery(fetchOriginQuery())),
            invalidate({ queryKey: qk.worktrees() }),
            invalidate({ queryKey: qk.remoteWorkerInfo() }),
            invalidate({ queryKey: qk.remoteWorktrees() }),
            invalidate({ queryKey: ["github"] }),
            invalidate({ queryKey: ["issueStatuses"] }),
            invalidate({ queryKey: qk.reviewRequests() }),
            invalidate({ queryKey: qk.wtState() }),
          ],
          { concurrency: "unbounded", discard: true },
        ).pipe(
          // The first-parent SHA set is not a TanStack query — it's a
          // module-level promise cache in core/git.ts, already dropped by
          // invalidateMainFirstParents() inside fetchOriginQuery. The
          // per-worktree ["wt"] wave is the expensive part, so start it on
          // the next timer turn instead of keeping the key handler/caller
          // parked behind every row's git/fs probes.
          Effect.tap(() =>
            forkLogged(
              "deferred wt invalidation",
              Effect.sleep(Duration.millis(50)).pipe(
                Effect.andThen(invalidate({ queryKey: ["wt"] })),
              ),
            ),
          ),
        ),
      );
    },
    /**
     * Force an origin refresh even if the marker query is still fresh.
     * Passive triggers use this so webhook/action events can advance local
     * main immediately instead of waiting out fetchOriginQuery's staleTime.
     */
    refreshOrigin(): Promise<void> {
      return Effect.runPromise(forceFetchOrigin().pipe(Effect.asVoid));
    },
    /**
     * Nuke every cached query — in-memory *and* the SQLite blob on
     * disk — drop the in-process `mainFirstParents` cache, then kick
     * off a `git fetch origin` to seed the refetch. Active observers
     * re-issue their own fetches immediately, so the UI returns to a
     * loading state and rebuilds from scratch.
     */
    clearAll(): Promise<void> {
      return Effect.runPromise(
        Effect.gen(function* () {
          yield* Effect.sync(() => qc.clear());
          yield* clearPersistedCache(CACHE_DB);
          yield* Effect.sync(invalidateMainFirstParents);
          // Not observed by any component, so it won't auto-refetch on
          // clear — kick it off explicitly so the first-parents cache gets
          // repopulated alongside the observed queries.
          yield* forkLogged(
            "clearAll origin refetch",
            queryClientEffect(() => qc.fetchQuery(fetchOriginQuery())),
          );
          // Belt-and-suspenders: `qc.clear()` removes cache entries, but
          // active observers sitting on `staleTime: Infinity` (notably
          // the AI summary) don't always re-trigger their queryFn
          // afterwards. Forcing a refetch on every active observer makes
          // "R" deterministic for the AI chain.
          yield* forkLogged(
            "clearAll active refetch",
            queryClientEffect(() => qc.refetchQueries({ type: "active" })),
          );
        }),
      );
    },
    /** Invalidate everything for a single worktree (useful after an action). */
    invalidateWorktree(slug: string): Promise<void> {
      return Effect.runPromise(invalidate({ queryKey: qk.wt(slug).all() }));
    },
    /**
     * Post-removal refresh — deliberately NOT `refreshAll`.
     *
     * A destroy changes exactly two things: which worktrees exist, and
     * the state file the detached child rewrites. Nothing about the
     * SURVIVING rows moved, and the removed ones are about to stop
     * existing, so `refreshAll`'s `["wt"]` wave — every field query for
     * every row on the board — re-derives state nobody touched at the
     * cost of ~10 git subprocesses per row, fired in one burst. Each
     * `Bun.spawn` costs the render thread its `posix_spawn`, which is
     * why the sweep's tail shows up as a stall rather than as
     * background work (`spawn` was 30% of main-thread self time in the
     * post-sweep profile).
     *
     * The rest arrives on its own: the github query is keyed BY the
     * branch list, so a shorter list is a different key and refetches
     * without being asked, and `watchWtStateFiles` covers the child's
     * writes. This is the belt to the watchers' braces, not the
     * mechanism — `watchWorktreesAdmin` / `watchWorktreeRoot` / the
     * lock-release chain all invalidate the list first (see
     * docs/architecture.md#freshness-model).
     */
    refreshAfterRemoval(): Promise<void> {
      return Effect.runPromise(
        Effect.all(
          [
            invalidate({ queryKey: qk.worktrees() }),
            invalidate({ queryKey: qk.wtState() }),
          ],
          { concurrency: "unbounded", discard: true },
        ),
      );
    },
    /**
     * Refresh stack relationships and the per-worktree diff queries.
     * Stack shape lives in the per-slug fork-base records, so
     * re-reading wtState surfaces a reparent or a restacked anchor;
     * invalidating `["wt"]` re-runs the per-base diff / sync queries
     * after a rebase rewrites history under a fixed parent.
     */
    refreshStack(): Promise<void> {
      return Effect.runPromise(
        Effect.all(
          [
            invalidate({ queryKey: qk.wtState() }),
            invalidate({ queryKey: ["wt"] }),
          ],
          { concurrency: "unbounded", discard: true },
        ),
      );
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
    fetchContributors(): Promise<readonly Contributor[]> {
      const opts = contributorsQuery();
      const cached = qc.getQueryData<readonly Contributor[]>(opts.queryKey);
      if (cached === undefined) {
        return Effect.runPromise(queryClientEffect(() => qc.fetchQuery(opts)));
      }
      const state = qc.getQueryState(opts.queryKey);
      const isStale =
        !state || Date.now() - state.dataUpdatedAt > (opts.staleTime as number);
      if (isStale) {
        Effect.runFork(
          queryClientEffect(() => qc.prefetchQuery(opts)).pipe(
            Effect.catch(() => Effect.void),
          ),
        );
      }
      return Promise.resolve(cached);
    },
    /**
     * Currently-authenticated GitHub login (or `null` when gh isn't
     * usable). Process-cached at the source — see
     * `fetchAuthenticatedLogin` in `core/github.ts`.
     */
    fetchMe(): Promise<string | null> {
      return Effect.runPromise(fetchAuthenticatedLogin());
    },
    /**
     * Invalidate the combined PR + merge-queue fetch. Use after an
     * action that mutates GitHub state (e.g. enabling auto-merge) so
     * the next render picks up the new server-side state without
     * waiting for the slow staleTime to expire.
     */
    refreshGithub(): Promise<void> {
      return Effect.runPromise(
        Effect.all(
          [
            invalidate({ queryKey: ["github"] }),
            invalidate({ queryKey: qk.reviewRequests() }),
          ],
          { concurrency: "unbounded", discard: true },
        ),
      );
    },
    /** Hide the exact current snapshot of a pinned review request. */
    dismissReviewRequest(url: string, updatedAt: string): Promise<void> {
      return writeWtState(() => dismissReviewRequestOnDisk(url, updatedAt));
    },
    /**
     * Invalidate the tmux-sessions query. Call after entering or
     * detaching from a session so the per-row indicator flips
     * immediately rather than waiting for the polling backstop.
     */
    refreshTmuxSessions(): Promise<void> {
      return Effect.runPromise(
        invalidate({ queryKey: tmuxSessionsQuery().queryKey }),
      );
    },
    /**
     * Invalidate the harness-sessions discovery for a slug. Call after
     * spawning / killing a harness session so the picker entries pick
     * up the new on-disk state. Hits all harnesses since codex /
     * opencode write to shared stores that could surface new entries.
     */
    refreshHarnessSessions(slug: string): Promise<void> {
      return Effect.runPromise(
        invalidate({
          queryKey: ["harnessSessions"],
          predicate: (q) => q.queryKey[2] === slug,
        }),
      );
    },
    /**
     * Persist a new primary harness selection and invalidate the
     * cached query so observers pick up the change.
     */
    setPrimaryHarness(
      id: import("../core/harness/index.ts").HarnessId,
    ): Promise<void> {
      return Effect.runPromise(
        Effect.gen(function* () {
          // A broken module on a hot update is an expected failure here,
          // not a defect: keep it on the typed channel.
          const { writePrimaryHarness } = yield* Effect.tryPromise({
            try: () => import("../core/harness/primary.ts"),
            catch: io.wrap("load primary harness"),
          });
          yield* Effect.sync(() => writePrimaryHarness(id));
          yield* invalidate({ queryKey: qk.primaryHarness() });
        }),
      );
    },
    /**
     * Cycle the primary harness to the next registered impl and
     * invalidate the cached query.
     */
    cyclePrimaryHarness(): Promise<
      import("../core/harness/index.ts").HarnessId
    > {
      return Effect.runPromise(
        Effect.gen(function* () {
          const { cyclePrimaryHarness } = yield* Effect.tryPromise({
            try: () => import("../core/harness/primary.ts"),
            catch: io.wrap("load primary harness"),
          });
          const next = yield* Effect.sync(() => cyclePrimaryHarness());
          yield* invalidate({ queryKey: qk.primaryHarness() });
          return next;
        }),
      );
    },
    /**
     * Invalidate the cached LLM summaries for `slug`. The query key
     * doesn't include the persisted-name list, so adding or removing
     * a named session needs an explicit nudge — without this, a
     * freshly-spawned session opens the picker showing "(no summary
     * yet)" for up to staleTime (~30s).
     */
    refreshClaudeSummaries(slug: string): Promise<void> {
      return Effect.runPromise(
        invalidate({ queryKey: qk.claudeSummaries(slug) }),
      );
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
     * (starting the naming harness), and `placeholderData: keepPreviousData`
     * keeps the prior summary on screen during the gap. Using
     * `invalidateQueries` instead of `removeQueries` is deliberate:
     * deleting the entry blanks the display because the observer's
     * keepPreviousData fallback only kicks in on a queryKey change,
     * not on an evicted same-key entry.
     */
    refreshAiSummary(slug: string): Promise<boolean> {
      return Effect.runPromise(
        Effect.gen(function* () {
          // The diffContext key is per-(slug, base) so a worktree can
          // have multiple cached entries (trunk, parent A, parent B…) as
          // its stack relationship evolves. Prefix-match to address every
          // cached entry for this slug; the row aggregator observes only
          // the *current* base, so on next render the live observer's
          // refetch produces the up-to-date value regardless of which
          // entries we touched here.
          const prefix = ["wt", slug, "diffContext"] as const;
          const existing = yield* Effect.sync(() =>
            qc.getQueriesData<DiffContext | null>({ queryKey: prefix }),
          );
          if (existing.length === 0 || existing.every(([, v]) => !v)) {
            return false;
          }
          // `invalidateQueries` awaits the refetch of any active observer
          // (default `refetchType: "active"`), so by the time this resolves
          // the diff context cache holds the new hash.
          yield* invalidate({ queryKey: prefix });
          const refreshed = yield* Effect.sync(() =>
            qc.getQueriesData<DiffContext | null>({ queryKey: prefix }),
          );
          if (refreshed.every(([, v]) => !v)) return false;
          // Invalidate (don't remove) the AI summary entry for each
          // still-present hash. Invalidate triggers an active-observer
          // refetch even with `staleTime: Infinity`, and the cache entry
          // stays put so `keepPreviousData` has data to show during the
          // gap.
          yield* Effect.all(
            refreshed
              .filter(([, ctx]) => !!ctx)
              .map(([, ctx]) =>
                invalidate({ queryKey: qk.aiSummary(ctx!.hash) }),
              ),
            { concurrency: "unbounded", discard: true },
          );
          return true;
        }),
      );
    },
    /**
     * Flip the archived flag for a slug. Optimistically patches the
     * archive set so the row reorders immediately under the cursor;
     * the disk write is sync and the post-settle invalidate just
     * confirms. Awaits the mutate call so `useWorktreeRows` has the
     * new state before the caller's next render — cursor-follow logic
     * relies on this.
     */
    async toggleArchived(key: string): Promise<{ archived: boolean }> {
      let result: { archived: boolean } | null = null;
      // `runOptimisticMutation` may re-apply a patch after a settling
      // refetch. Capture the intended state on the first application and
      // make every later application idempotent; a literal toggle here
      // re-added the row to the active list even though archive.json had
      // correctly persisted it as archived.
      let intendedArchived: boolean | null = null;
      await mutate<readonly string[]>({
        filter: { queryKey: qk.archive() },
        patch: (prev) => {
          const set = new Set(prev ?? []);
          intendedArchived ??= !set.has(key);
          return patchArchivedKeys(prev, key, intendedArchived);
        },
        run: io.sync("toggle archive", () => {
          // Disk write is synchronous; the mutate pipeline just awaits
          // the Effect. Errors propagate as a typed failure and trigger
          // the rollback path.
          result = toggleArchivedOnDisk(key);
          // Disk is authoritative if another process changed the ledger
          // between the cached read and this serialized write.
          intendedArchived = result.archived;
        }),
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
        run: io.sync("archive slug", () => {
          archiveOnDisk(slug);
        }),
      });
    },
    /**
     * Assign (or clear, with `null`) a slug's section. Order is reset
     * to the bottom of the target group — the picker convention. Awaits
     * invalidation so cursor-follow can read fresh rows.
     */
    setSection(key: string, section: string | null): Promise<void> {
      // Narration is NOT emitted here. `wt section` writes this same
      // field from another process, so the only place that sees every
      // move is the wtstate diff in `useWtStateEvents` — emitting at
      // the call site too would double-log ours and still miss theirs.
      // Marking the write first lets that diff tell "the human just
      // pressed `l`" (firehose) from "something else rearranged their
      // board" (attention).
      return writeWtState(() => {
        markSelfSectionWrite(key, section);
        setWorktreeSectionOnDisk(key, section);
      });
    },
    /**
     * Assert (or clear, with `null`) a slug's work status — the TUI
     * (`u` picker) leg of `wt status`. The wtState invalidation is what
     * re-runs the status-first sort; awaited so cursor-follow reads
     * fresh rows.
     */
    setWorkStatus(
      slug: string,
      record: WorkStatusRecord | null,
    ): Promise<void> {
      return writeWtState(() => {
        setSlugWorkStatusOnDisk(slug, record);
      });
    },
    /**
     * Set (or clear, with `null`) a worktree's tracker-id override —
     * the same per-slug record `wt issue <slug> --id` writes. Pure
     * wtstate, so the wtState query is the only thing to invalidate.
     */
    setIssueId(slug: string, id: string | null): Promise<void> {
      return writeWtState(() => {
        setSlugIssueIdOnDisk(slug, id);
      });
    },
    /**
     * Record (or clear, with `null`) a worktree's fork base — the same
     * per-slug record `wt new --base` writes, and the record stacks are
     * inferred from. Record only: no rebase happens. Anchors the
     * fork-point sha at merge-base (best-effort, like `wt base set`).
     * Invalidates wtState (row relationship) AND the slug's `["wt"]`
     * queries — diff context and sync counts are computed against the
     * base, so they must re-run under the new one.
     */
    setBase(wt: Worktree, branch: string | null): Promise<void> {
      return Effect.runPromise(
        Effect.gen(function* () {
          if (branch) {
            const mb = yield* gitRun(
              ["merge-base", wt.branch, branch],
              wt.path,
            );
            const sha = mb.exitCode === 0 ? mb.stdout.trim() : "";
            yield* io.sync("set worktree base", () =>
              setSlugBaseOnDisk(wt.slug, { branch, sha: sha || undefined }),
            );
          } else {
            yield* io.sync("clear worktree base", () =>
              setSlugBaseOnDisk(wt.slug, null),
            );
          }
          yield* Effect.all(
            [
              invalidate({ queryKey: qk.wtState() }),
              invalidate({ queryKey: qk.wt(wt.slug).all() }),
            ],
            { concurrency: "unbounded", discard: true },
          );
        }),
      );
    },
    /**
     * Place a slug at the top or bottom of a section. Used by the
     * unified Shift+J/K cross-section nudge so the moved row lands
     * adjacent to where it was (top of next section, bottom of prev).
     */
    placeSlug(
      slug: string,
      section: string | null,
      position: "top" | "bottom",
    ): Promise<void> {
      return writeWtState(() => placeSlugOnDisk(slug, section, position));
    },
    /**
     * Swap two slugs' order values within a single section bucket.
     * `bucketDisplay` must be the bucket's current display order — the
     * write path renormalizes the bucket against this list before
     * swapping, so any unstated entries get materialized cleanly.
     */
    swapOrder(
      slugA: string,
      slugB: string,
      section: string | null,
      bucketDisplay: readonly string[],
    ): Promise<void> {
      return writeWtState(() =>
        swapOrdersOnDisk(slugA, slugB, section, bucketDisplay),
      );
    },
    /**
     * Rename a section across every slug that references it. Awaits
     * invalidation so the renamed section and its members are visible
     * in the next render.
     */
    renameSection(oldName: string, newName: string): Promise<void> {
      return writeWtState(() => renameSectionOnDisk(oldName, newName));
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
    moveGroupPast(
      key: string,
      pastKey: string,
      side: "before" | "after",
      visualOrder: readonly string[] = [],
    ): Promise<boolean> {
      return Effect.runPromise(
        Effect.gen(function* () {
          const moved = yield* io.sync("move section group", () =>
            moveGroupPastOnDisk(key, pastKey, side, visualOrder),
          );
          if (moved) yield* invalidate({ queryKey: qk.wtState() });
          return moved;
        }),
      );
    },
    /**
     * Toggle the per-worktree automations pause flag (persisted in
     * wtstate; Ctrl+A on a non-stack row). Returns the new paused
     * state. The automations engine reads the flag through the wtState
     * query, so the invalidation is what makes the toggle take effect.
     */
    toggleAutomationsPaused(slug: string): Promise<boolean> {
      return writeWtState(() => toggleSlugAutomationsPausedOnDisk(slug));
    },
    /**
     * Toggle the automations pause on an ARCHIVED row (Ctrl+A in the
     * `h` view). Writes the removed-history entry, since the per-slug
     * record was reaped with the worktree while post-merge `external`
     * automations were not. Null when the slug isn't in the history.
     */
    toggleRemovedAutomationsPaused(slug: string): Promise<boolean | null> {
      return Effect.runPromise(
        Effect.gen(function* () {
          const paused = yield* io.sync("toggle removed automations", () =>
            toggleRemovedAutomationsPausedOnDisk(slug),
          );
          if (paused !== null) yield* invalidate({ queryKey: qk.wtState() });
          return paused;
        }),
      );
    },
    /**
     * Toggle the whole-stack automations pause (Ctrl+A on a stack
     * member or its folded header). Keyed by stackId (the root branch)
     * so members stacked on later are covered by the same pause; the
     * current members' per-slug flags are mirrored too, so the pause
     * survives the stack re-rooting when the root lands.
     */
    toggleStackAutomationsPaused(
      stackId: string,
      memberSlugs: readonly string[],
    ): Promise<boolean> {
      return writeWtState(() =>
        toggleStackAutomationsPausedOnDisk(stackId, memberSlugs),
      );
    },
    /**
     * Fold or unfold a section in the list (persisted). Returns the new
     * folded state. The list re-derives its items from the refreshed
     * `wtState`, collapsing/expanding the section's rows.
     */
    toggleSectionFold(sectionKey: string): Promise<boolean> {
      // File-only: a fold is view state and TAB is pressed constantly,
      // so this would drown the activity pane. It's in the log because
      // a folded section makes its rows vanish from the list, which is
      // indistinguishable from them having moved — the daily log is
      // where that question gets settled.
      return writeWtState(() => {
        const folded = toggleSectionFoldedOnDisk(sectionKey);
        createLogger("[app]").debug(
          `section ${folded ? "folded" : "unfolded"}: ${sectionKey}`,
        );
        return folded;
      });
    },
    /** Set a section's fold state without risking an accidental re-toggle. */
    setSectionFolded(sectionKey: string, folded: boolean): Promise<boolean> {
      return writeWtState(() => setSectionFoldedOnDisk(sectionKey, folded));
    },
  };
}
