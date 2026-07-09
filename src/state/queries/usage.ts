import { queryOptions } from "@tanstack/react-query";

import { readClaudeUsage, type ClaudeUsage } from "../../core/claude-usage.ts";
import { readCodexUsage, type CodexUsage } from "../../core/harness/codex-usage.ts";
import {
  readOpencodeCost,
  type OpencodeCost,
} from "../../core/harness/opencode-usage.ts";

import { qk } from "../keys.ts";

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

/**
 * Codex rate-limit usage (5h/7d %), parsed from the newest rollout's
 * latest `token_count` event. No HTTP — purely on-disk. Same cadence as
 * the claude usage read; gated to the codex primary at the call site.
 */
export const codexUsageQuery = () =>
  queryOptions({
    queryKey: qk.codexUsage(),
    queryFn: async (): Promise<CodexUsage | null> => readCodexUsage(),
    staleTime: 30_000,
    refetchInterval: 60_000,
  });

/**
 * OpenCode spend (5h/7d $), summed from its message-cost rows. Windows
 * slide with wall-clock, so this is recomputed each refetch rather than
 * cached against a file mtime.
 */
export const opencodeCostQuery = () =>
  queryOptions({
    queryKey: qk.opencodeCost(),
    queryFn: async (): Promise<OpencodeCost | null> =>
      readOpencodeCost(Date.now()),
    staleTime: 30_000,
    refetchInterval: 60_000,
  });
