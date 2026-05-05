/**
 * Hooks returning the sets of slugs that currently own live wt tmux
 * sessions, per kind. `useActiveSessions` is the claude variant —
 * used by the worktree list panel (per-row indicator) and the
 * details-pane claude row. `useActiveDiffSessions` is the diff
 * variant — used by the Shift+F11 kill-confirm hint. One global
 * query powers both — see `tmuxSessionsQuery` in `state/queries.ts`.
 */
import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";

import { tmuxSessionsQuery } from "../../state/queries.ts";

const EMPTY: ReadonlySet<string> = new Set();

export function useActiveSessions(): ReadonlySet<string> {
  const q = useQuery(tmuxSessionsQuery());
  return useMemo(() => {
    const list = q.data?.claude;
    if (!list || list.length === 0) return EMPTY;
    return new Set(list);
  }, [q.data]);
}

export function useActiveDiffSessions(): ReadonlySet<string> {
  const q = useQuery(tmuxSessionsQuery());
  return useMemo(() => {
    const list = q.data?.diff;
    if (!list || list.length === 0) return EMPTY;
    return new Set(list);
  }, [q.data]);
}
