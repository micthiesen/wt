/**
 * Hook returning the set of slugs that currently own a live wt tmux
 * session. Used by the worktree list panel (per-row indicator) and the
 * details-pane claude row (session-attached hint). One global query
 * powers both — see `tmuxSessionsQuery` in `state/queries.ts`.
 */
import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";

import { tmuxSessionsQuery } from "../../state/queries.ts";

const EMPTY: ReadonlySet<string> = new Set();

export function useActiveSessions(): ReadonlySet<string> {
  const q = useQuery(tmuxSessionsQuery());
  return useMemo(() => {
    if (!q.data || q.data.length === 0) return EMPTY;
    return new Set(q.data);
  }, [q.data]);
}
