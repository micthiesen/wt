import type { QueryCacheNotifyEvent, QueryClient } from "@tanstack/react-query";

import { createLogger } from "../core/logger.ts";
import { formatDuration } from "../core/text.ts";

/**
 * Surface external (network-hitting) query fetches in the event log so
 * it's obvious when wt is talking to GitHub or the git remote. Local
 * git/fs queries are omitted — they're fast and would drown everything
 * else out. Logs start + completion + failure; duration on completion
 * so slow fetches are visible as slow.
 */
const REMOTE_LABELS: Record<string, { source: string; label: string }> = {
  github: { source: "[gh]", label: "GitHub" },
  reviewRequests: { source: "[gh]", label: "review requests" },
  fetchOrigin: { source: "[origin]", label: "git origin" },
};

const ghLog = createLogger("[gh]");
const originLog = createLogger("[origin]");

function loggerFor(source: string) {
  return source === "[gh]" ? ghLog : originLog;
}
export function attachFetchLogs(client: QueryClient): () => void {
  const starts = new Map<string, number>();
  // Sources currently in a failing streak (keyed by label). The FIRST
  // failure is attention-worthy (`event.err` surfaces in the attention
  // feed by level); repeats during a sustained outage — e.g. a rate-limit
  // storm refetching every backstop interval — stay on the dim firehose
  // so the curated feed gets one line per outage, not one per poll.
  // Recovery logs once too, closing the loop.
  const failing = new Set<string>();
  return client.getQueryCache().subscribe((event: QueryCacheNotifyEvent) => {
    if (event.type !== "updated") return;
    const first = event.query.queryKey[0];
    if (typeof first !== "string") return;
    const meta = REMOTE_LABELS[first];
    if (!meta) return;
    const log = loggerFor(meta.source);
    const action = event.action;
    if (action.type === "fetch") {
      starts.set(event.query.queryHash, Date.now());
      log.event.dim(`fetching ${meta.label}...`);
    } else if (action.type === "success") {
      const start = starts.get(event.query.queryHash);
      // `success` also fires for non-fetch data writes — the persister's
      // startup restore and optimistic `setQueriesData` patches dispatch
      // one per matching cache entry (every stale `["github", <old
      // branches>]` key included), which showed up as 80+ identical
      // "fetched GitHub" lines in a single second. Only log the
      // completion of a fetch we saw start.
      if (start === undefined) return;
      starts.delete(event.query.queryHash);
      if (failing.delete(meta.label)) {
        log.event.ok(`${meta.label} fetch recovered`, { toast: true });
      }
      log.event.dim(`fetched ${meta.label} (${formatDuration(Date.now() - start)})`);
    } else if (action.type === "error") {
      starts.delete(event.query.queryHash);
      const err = action.error;
      const msg = err instanceof Error ? err.message : String(err);
      if (failing.has(meta.label)) {
        log.event.dim(`still failing to fetch ${meta.label}: ${msg}`);
      } else {
        failing.add(meta.label);
        log.event.err(`failed to fetch ${meta.label}: ${msg}`);
      }
    }
  });
}
