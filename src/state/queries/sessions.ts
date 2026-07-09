import { queryOptions } from "@tanstack/react-query";

import {
  getHarness,
  type HarnessId,
  type HarnessSession,
} from "../../core/harness/index.ts";
import {
  type ClaudeSessionEntry,
  listSessions as listTmuxSessions,
} from "../../core/tmux.ts";

export type { ClaudeSessionEntry };

import { qk } from "../keys.ts";
import { STALE } from "./shared.ts";

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
 * parallel queries. Push triggers do the fast work: explicit
 * invalidation fires on enter/detach/kill, and the claude-registry
 * watcher invalidates on claude process start/exit. The 5s interval is
 * a backstop for lifecycle events with no trigger (a shell/diff
 * session's process exiting on its own, external `tmux kill-session`).
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
    refetchInterval: 5_000,
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
      const { readPrimaryHarness } = await import("../../core/harness/primary.ts");
      return readPrimaryHarness();
    },
    staleTime: Infinity,
  });
