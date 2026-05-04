import { QueryClient } from "@tanstack/react-query";
import { experimental_createQueryPersister } from "@tanstack/query-persist-client-core";

import { config } from "../core/config.ts";

import { createSqliteAsyncStorage } from "./persister.ts";

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
const CACHE_BUSTER = "v6";
const STORAGE_PREFIX = "wt";
const MAX_CACHE_AGE_MS = 30 * 24 * 60 * 60 * 1000;

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
  /** Stop the persister, close the storage handle. */
  shutdown(): void;
};

export function createWtQueryClient(): WtQueryClient {
  const storage = createSqliteAsyncStorage(CACHE_DB);
  const persister = experimental_createQueryPersister<string>({
    storage,
    buster: CACHE_BUSTER,
    maxAge: MAX_CACHE_AGE_MS,
    prefix: STORAGE_PREFIX,
    // Persister wraps every queryFn invocation; high-frequency
    // polling queries (lock: 2s while held, claude: 5s) would
    // otherwise burn one INSERT OR REPLACE per poll for data with
    // zero cross-session value. Worse, restoring stale lock state on
    // startup mis-classifies the worktree as "busy" until the first
    // refetch lands. Filter them out so they live purely in-memory.
    filters: {
      predicate: (query) => {
        const key = query.queryKey;
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
        // something looks off.
        retry: false,
        refetchOnWindowFocus: false,
        refetchOnReconnect: false,
        // Per-query persistence: every queryFn call goes through the
        // persister wrapper. Restored entries skip the queryFn on first
        // observe; subsequent calls hit storage on success and retrieve
        // on cold cache.
        persister: persister.persisterFn,
      },
    },
  });

  // Pre-warm: walk every persisted entry and populate the cache before
  // first paint. Without this, queries would only restore as their
  // observers mount, and the first frame would show empty placeholders
  // for everything. The runtime races this against a small budget so a
  // huge cache doesn't block startup.
  const restored = persister.restoreQueries(client);

  return {
    client,
    restored,
    shutdown(): void {
      client.getQueryCache().clear();
      client.unmount();
      storage.close();
    },
  };
}
