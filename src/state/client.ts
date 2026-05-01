import { QueryClient } from "@tanstack/react-query";
import { persistQueryClient } from "@tanstack/query-persist-client-core";

import { config } from "../core/config.ts";

import { createSqlitePersister } from "./persister.ts";

export const CACHE_DB = config.paths.cacheDb;

// Bust the persisted cache when the schema / query shape changes.
// v2: aiSummaryQuery now returns `{title, description}` instead of a
// raw string; old entries can't be rehydrated cleanly.
// v3: aiSummaryQuery added a required `brief` field; old entries
// would deserialise without it and break consumers.
const CACHE_BUSTER = "v3";

/**
 * Build a QueryClient with TUI-friendly defaults and wire up the
 * SQLite persister. Returns the client plus a `restored` promise that
 * resolves once hydration is complete, so callers can decide whether
 * to render immediately (showing stale data) or wait.
 */
export type WtQueryClient = {
  client: QueryClient;
  restored: Promise<void>;
  /** Stop the persister subscription, clear timers, close the db. */
  shutdown(): void;
};

export function createWtQueryClient(): WtQueryClient {
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
      },
    },
  });

  const persister = createSqlitePersister(CACHE_DB);
  const [unsubscribe, restored] = persistQueryClient({
    queryClient: client,
    persister,
    // Long enough that content-addressed AI summaries (`aiSummaryQuery`)
    // survive across the lifetime of a typical worktree without forcing
    // a regen. Other queries restore-then-immediately-refetch on their
    // own staleTime, so a longer maxAge has no downside for them.
    maxAge: 30 * 24 * 60 * 60 * 1000,
    buster: CACHE_BUSTER,
  });

  return {
    client,
    restored,
    shutdown(): void {
      unsubscribe();
      client.getQueryCache().clear();
      client.unmount();
      persister.close();
    },
  };
}
