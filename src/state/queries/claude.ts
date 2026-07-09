import { queryOptions } from "@tanstack/react-query";

import { readRegistry, type RegistrySession } from "../../core/harness/claude/registry.ts";
import { wtSessionUuid } from "../../core/harness/claude/jsonl.ts";
import { listClaudeNames } from "../../core/harness/claude/names.ts";
import { readSummariesForSessions, type SessionSummary } from "../../core/harness/claude/summaries.ts";
import type {
  Worktree,
} from "../../core/types.ts";

import { qk } from "../keys.ts";

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
 * Sized generously — the watcher is the mechanism, this only bounds a
 * missed event.
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
    refetchInterval: 15_000,
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
