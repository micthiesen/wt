import { hashKey, notifyManager, QueryClient } from "@tanstack/react-query";
import { experimental_createQueryPersister, type PersistedQuery } from "@tanstack/query-persist-client-core";

import { config } from "../core/config.ts";

import { createSqliteAsyncStorage, type AsyncStorageDb } from "./persister.ts";

export const CACHE_DB = config.paths.cacheDb;

// Bust the persisted cache when the schema / query shape changes.
// v2: aiSummaryQuery now returns `{title, description}` instead of a
// raw string; old entries can't be rehydrated cleanly.
// v3: aiSummaryQuery added a required `brief` field; old entries
// would deserialise without it and break consumers.
// v4: aiSummaryQuery moved from hash-keyed to slug-keyed with a
// separate hash-keyed memo. Old `["aiSummary", <hash>]` entries
// would never be looked up under the new shape and would just sit
// dead in the persisted blob.
// v5: aiSummary is hash-keyed again (no slug indirection, no memo
// family). The value shed its `hash` field; old slug-keyed entries
// would never be observed, and old aiSummaryMemo entries are dead
// weight.
// v6: switched from whole-blob `persistQueryClient` to the
// `experimental_createQueryPersister` per-query model. Storage layout
// changed (one row per query, prefixed `wt-<queryHash>`), and the
// older single-row `wt.cache.v1` blob will never be read.
// v7: TmuxSessionsData replaced the `claudeSlugs`/`codex`/`opencode`
// fields with a single `slugsByHarness` record; a restored v6 entry
// would lack it and break consumers that index `slugsByHarness[id]`.
// v8: worktree discovery now ignores non-main worktrees outside the
// configured worktree_root; old external-tool rows should not restore.
// v9: TmuxSessionsData gained the required `remote` field (hub mode's
// SSH wrapper sessions); a restored v8 entry would lack it and lie to
// consumers about the declared non-optional type.
// v10: hub mode removed — TmuxSessionsData dropped `remote` again, and
// hub-only queries (task focus) should not restore. The same release
// also added required work-status fields (`workState` et al.) to the
// persisted RemoteWorktreeSummary shape — independently bust-worthy,
// covered by this same bump.
// v11: remote inventories are keyed per SSH host and each summary captures
// its complete endpoint. The old singleton-key rows cannot safely route an
// operation once more than one remote exists.
// v12: PullRequest gained `unresolvedThreadsTotal` (unresolved review
// threads regardless of author). A cached PR from v11 has no such field,
// and a missing count would render as "nothing outstanding" — the exact
// absence-reads-as-clean failure the field was added to fix.
// v17: `DevServerStatus` gained `restarts`. A restored v16 entry
// lacks it, so a row mid-restart-loop would render as merely
// starting until the next poll — the exact confusion the field
// exists to end.
// v16: `DevServerStatus` gained `rebasedSince` — a restored v15 entry
// has it absent, and absent is the tri-state's "unknown", which the
// row renders as nothing. Harmless for a poll, but the entry would
// also be the one shown at boot, i.e. exactly when a rebase that
// happened while wt was closed is most worth seeing.
// v15: `RemoteWorktreeSummary` gained `workBlockedOn` — the external
// merge gate. A restored v14 entry has the field absent, which a
// consumer reading it as "no gate" cannot distinguish from a remote
// that genuinely has none; for a field whose whole job is to say DO
// NOT MERGE, that is the wrong way to be wrong.
// v14: `DevServerStatus` gained `since` and `waiting`. A restored v13
// entry has neither, and the row reads them unconditionally — an
// undefined `since` would render `NaN` where the starting age goes.
// v13: `SyncState.remote` changed MEANING, not shape — it now counts
// against `origin/<branch>` rather than `@{u}` (which wt points at the
// base). Same field, same type, different answer: a restored v12 entry
// would feed ahead-of-base numbers straight into the destroy guard,
// which is the bug the change exists to fix.
// v18: GithubData.mergeQueue changed MEANING without changing shape. It
// was populated from the repo's DEFAULT-branch queue, which is the wrong
// queue whenever worktrees target something else — on a repo whose queue
// lives on `staging` it was empty on every poll, so the position badge
// never rendered. A restored v17 entry's empty map is indistinguishable
// from a fresh "nothing is queued", which is exactly the case the buster
// exists for.
// v19: `PullRequest.checks` / `failedChecks` changed MEANING without
// changing shape. The rollup is HISTORY — GitHub keeps every check run
// for a head sha forever — so a superseded failure (a retried flake, a
// cancelled run, a job that failed while the PR was a draft) pinned the
// badge red for the life of the branch on a PR that was actually green.
// The derivation now collapses to the newest run per context, and a
// restored v18 "fail" is indistinguishable from a fresh one.
// v20: `RemoteWorktreeSummary` gained `workVerifyAfterMerge` — a check
// the branch owes once it is deployed. A restored v19 entry has it
// absent, which reads as "nothing owed"; on a merged remote row that is
// the one answer that releases the checkout to the sweep, taking the
// context the check needed with it.
// v21: `SyncState`'s shape is unchanged and its MEANING is not. Both
// counts now measure against the main clone's trunk tip when the
// checkout holds that commit, instead of against the checkout's own
// `origin/<trunk>` — which under `rift` is frozen at clone time and was
// inflating "ahead" by every trunk commit that landed after it (156 vs
// a true 14 on one live row). A restored v20 entry carries the old
// number in the new shape, and nothing distinguishes them.
// v23: every BASE-DERIVED entry changed meaning without changing shape.
// `effectiveBaseOrTrunk` was resolving the bare trunk name a fork-base
// record stores (`"staging"`) to the checkout's LOCAL branch of that
// name, which under rift is frozen at clone time — so the sync counts,
// the first-commit title, the conflict probe and the diff context were
// all measured against a base 97 to 383 commits behind. A restored
// entry is indistinguishable from a fresh one, which is what makes this
// a bust rather than a shape bump. (v22 busted `firstCommit` alone for
// the narrower stale-remote-ref version of the same failure.)
// v26: `RemoteWorktreeSummary` gained `section` under the former remote-owned
// layout model. Restoring v25 rows would have flashed false grouping then.
// v27: `merged` / `gone` are now keyed on the BRANCH, not the slug alone.
// The new key simply misses the old entries, so nothing stale is served
// either way — the bust is to evict them rather than leave every user
// carrying an orphaned per-slug verdict forever, and to clear the ones
// already poisoned: a repointed row served the previous branch's
// "merged" for as long as the entry survived, which fired
// builtin:delete-branch against a branch with an OPEN PR.
// v28: remote `section` changed authority without changing shape. A worker's
// persisted section must never flash before the controller-owned layout joins
// it, so discard inventories written under the former ownership model.
// v29: `RemoteWorktreeSummary` gained the worker-observed dev-server state.
// An old cached row has no value and would briefly render as falsely stopped.
// v30: remote inventory now restores the canonical nested snapshot shape
// (`status` and `work` objects) rather than the former flattened CLI fields.
// v31: `TmuxSessionsData.slugsByHarness` gained the required `opencode`
// member. A restored v30 entry lacks it, and consumers enumerating harnesses
// can otherwise fail during the first render before the live poll replaces it.
// v32: `TmuxSessionsData` gained `harnessSessionIds`, the exact resumed UUID
// stamped on a live single-slot tmux session. Restoring v31 would briefly
// revive the file-recency guess this field replaces.
// v33: AiSummary uses one title and no separate brief. Preserve valid v32
// summaries at deserialization: manual-only naming cannot regenerate them
// automatically. Other query families follow ordinary cache busting.
const CACHE_BUSTER = "v33";
const STORAGE_PREFIX = "wt";
const MAX_CACHE_AGE_MS = 30 * 24 * 60 * 60 * 1000;

function deserializeQuery(serialized: string): PersistedQuery {
  const persisted: PersistedQuery = JSON.parse(serialized);
  if (!persisted || !persisted.state || !Array.isArray(persisted.queryKey)) {
    throw new Error("Invalid persisted query");
  }
  if (persisted.buster !== "v32") return persisted;

  const key = persisted.queryKey;
  const summaryKey = typeof key[1] === "string" && key[1].length > 0 && (
    (key.length === 2 && key[0] === "aiSummary") ||
    (key.length === 3 && key[0] === "wt" && key[2] === "manualSummary")
  );
  const data = persisted.state.data;
  if (!summaryKey || persisted.queryHash !== hashKey(key) ||
      !Number.isFinite(persisted.state.dataUpdatedAt) || persisted.state.dataUpdatedAt <= 0 ||
      !data || typeof data !== "object" ||
      !("title" in data) || !(data.title === null || typeof data.title === "string") ||
      !("description" in data) || typeof data.description !== "string" ||
      !("brief" in data) || typeof data.brief !== "string") {
    return persisted;
  }
  // Preserve identity and all timestamps. The persister still applies maxAge;
  // migration must not extend a cached name's lifetime or invoke its model.
  return {
    ...persisted,
    buster: "v33",
    state: { ...persisted.state, data: { title: data.title, description: data.description } },
  };
}

/**
 * Build a QueryClient with TUI-friendly defaults and wire up the
 * per-query SQLite persister. Returns the client plus a `restored`
 * promise that resolves once every persisted query has been re-hydrated
 * into the cache, so callers can decide whether to render immediately
 * (showing stale data) or wait.
 */
export type WtQueryClient = {
  client: QueryClient;
  restored: Promise<void>;
  /**
   * Drop a query from both the in-memory cache and the SQLite blob.
   * Used at startup to evict orphaned entries persisted under keys
   * whose shape changed across a wt upgrade — without it the entry
   * sits dead in the persister forever (or until maxAge) and gets
   * pre-warmed back into memory on every launch.
   */
  evict(queryKey: readonly unknown[]): void;
  /** Stop the persister, close the storage handle. */
  shutdown(): Promise<void>;
};

export function createWtQueryClient(storage: AsyncStorageDb = createSqliteAsyncStorage(CACHE_DB)): WtQueryClient {
  const persister = experimental_createQueryPersister<string>({
    storage,
    deserialize: deserializeQuery,
    buster: CACHE_BUSTER,
    maxAge: MAX_CACHE_AGE_MS,
    prefix: STORAGE_PREFIX,
    // Persister wraps every queryFn invocation; high-frequency
    // polling queries (lock: 2s while held, claude: 15s) would
    // otherwise burn one INSERT OR REPLACE per poll for data with
    // zero cross-session value. Worse, restoring stale lock state on
    // startup mis-classifies the worktree as "busy" until the first
    // refetch lands. `claudeRegistry` follows the same rule: its
    // pid→busy/idle map is only meaningful within one wt uptime
    // window, and a restored entry would flash a prior run's pids
    // (often dead, possibly recycled) until the polling backstop
    // catches up. Filter them out so they live purely in-memory.
    filters: {
      predicate: (query) => {
        const key = query.queryKey;
        // Canonical state is local and already durable. A stale second copy
        // can hide a manual-title lock and start unwanted background naming.
        if (key[0] === "wtState") return false;
        if (key[0] === "claudeRegistry") return false;
        // Session discovery is ephemeral (live-session state, polled for
        // codex/opencode) and worthless across runs — restoring it would
        // flash stale sessions on boot. Keep it in-memory only.
        if (key[0] === "harnessSessions") return false;
        // Summaries are keyed only by slug, but the VALUE is derived from
        // the live persisted-name list at fetch time. A restored entry
        // pre-warms a previous run's name set (ghost/missing sessions)
        // until the staleTime refetch — recompute fresh each run instead.
        if (key[0] === "claudeSummaries") return false;
        // Perf snapshots are a live process table sampled every 2s while
        // the `P` overlay is open. Restoring one would paint a previous
        // run's pids (dead, possibly recycled), and persisting at that
        // cadence is precisely the write amplification this filter guards.
        if (key[0] === "perf") return false;
        if (key.length < 3 || key[0] !== "wt") return true;
        const slot = key[2];
        return slot !== "lock" && slot !== "claude";
      },
    },
  });

  const client = new QueryClient({
    defaultOptions: {
      queries: {
        // Persisted data is reused across runs; we rely on per-query
        // `staleTime` to drive refetch rather than gcTime-on-unmount.
        gcTime: 24 * 60 * 60 * 1000,
        // Retries are annoying in a TUI — the user will hit `r` if
        // something looks off. The one exception is deliberate and lives
        // a layer down: `core/github/fetch.ts` retries an individual
        // chunk of its batched fetch on transient 5xx/timeout, because
        // that fetch is a background poll with nobody watching and
        // `keepPreviousData` holds the last good badges on screen
        // throughout. Retrying HERE would re-run the whole fan-out.
        retry: false,
        refetchOnWindowFocus: false,
        refetchOnReconnect: false,
        // Per-query persistence: every queryFn call goes through the
        // persister wrapper. Restored entries skip the queryFn on first
        // observe; subsequent calls hit storage on success and retrieve
        // on cold cache.
        persister: async (queryFn, context, query) => {
          if (context.meta?.forceFresh === true) {
            // Explicit regeneration must await the fresh result, even on a
            // cold cache. Still persist it after Query has installed the data.
            const result = await queryFn(context);
            notifyManager.schedule(() => { void persister.persistQuery(query); });
            return result;
          }
          return persister.persisterFn(queryFn, context, query);
        },
      },
    },
  });

  // Pre-warm: walk every persisted entry and populate the cache before
  // first paint. Without this, queries would only restore as their
  // observers mount, and the first frame would show empty placeholders
  // for everything. The runtime races this against a small budget so a
  // huge cache doesn't block startup.
  const evictedDuringRestore = new Set<string>();
  let restoring = true;
  let closed = false;
  const restored = (async () => {
    // Disk reads may finish after startup's restore budget. Stage the snapshot
    // so it cannot resurrect evicted keys or overwrite a newer live fetch.
    const snapshot = new QueryClient();
    try {
      await persister.restoreQueries(snapshot);
      if (closed) return;
      for (const query of snapshot.getQueryCache().getAll()) {
        // restoreQueries does not apply the persister's write predicate.
        // Ignore legacy snapshots of this now-canonical-only query as well.
        if (query.queryKey[0] === "wtState") continue;
        if (evictedDuringRestore.has(query.queryHash)) continue;
        if (client.getQueryState(query.queryKey)?.dataUpdatedAt) continue;
        client.setQueryData(query.queryKey, query.state.data, { updatedAt: query.state.dataUpdatedAt });
      }
    } finally {
      snapshot.clear();
      evictedDuringRestore.clear();
      restoring = false;
    }
  })();

  return {
    client,
    restored,
    evict(queryKey): void {
      if (restoring) evictedDuringRestore.add(hashKey([...queryKey]));
      // Mirror what the persister does on write: storage key = prefix
      // + hashKey(queryKey). hashKey is the same hasher the QueryCache
      // uses to dedupe observers, so it's the only correct way to
      // address a persisted row from outside.
      client.removeQueries({ queryKey: [...queryKey], exact: true });
      storage.removeItem(`${STORAGE_PREFIX}-${hashKey([...queryKey])}`);
    },
    async shutdown(): Promise<void> {
      closed = true;
      client.getQueryCache().clear();
      client.unmount();
      await storage.close();
    },
  };
}
