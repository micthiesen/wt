/**
 * Hooks returning the sets of slugs that currently own live wt tmux
 * sessions, per kind. `useActiveSessions` is the claude variant —
 * used by the worktree list panel (per-row indicator) and the
 * details-pane claude row. `useActiveDiffSessions` /
 * `useActiveShellSessions` power the Shift+F11 / Shift+F10
 * kill-confirm hints. One global query powers all three — see
 * `tmuxSessionsQuery` in `state/queries.ts`.
 *
 * `useClaudeSessionsBySlug` exposes the multi-session shape: a Map
 * from slug to the list of session names live on it (`null` is the
 * primary). Drives the sessions picker and the row count badge.
 */
import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";

import { tmuxSessionsQuery } from "../../state/queries.ts";

const EMPTY: ReadonlySet<string> = new Set();
const EMPTY_NAMES: ReadonlyArray<string | null> = [];

export function useActiveSessions(): ReadonlySet<string> {
  const q = useQuery(tmuxSessionsQuery());
  return useMemo(() => {
    const list = q.data?.claudeSlugs;
    if (!list || list.length === 0) return EMPTY;
    return new Set(list);
  }, [q.data]);
}

/**
 * Map from slug → list of live claude session names on that slug.
 * `null` entry = primary session. Entries are deduped and stable in
 * tmux-listed order. Empty slugs are absent (no key) rather than
 * present-with-empty-array, so consumers can `.get(slug) ?? []`.
 */
export function useClaudeSessionsBySlug(): ReadonlyMap<
  string,
  ReadonlyArray<string | null>
> {
  const q = useQuery(tmuxSessionsQuery());
  return useMemo(() => {
    const list = q.data?.claude;
    if (!list || list.length === 0) return new Map();
    const map = new Map<string, (string | null)[]>();
    for (const entry of list) {
      const arr = map.get(entry.slug);
      if (arr) arr.push(entry.name);
      else map.set(entry.slug, [entry.name]);
    }
    return map;
  }, [q.data]);
}

/**
 * Live claude session names for one slug. `null` = primary. Returns
 * a stable empty array when the slug has no live sessions so memoized
 * consumers don't churn.
 */
export function useClaudeSessionsForSlug(
  slug: string,
): ReadonlyArray<string | null> {
  const map = useClaudeSessionsBySlug();
  return map.get(slug) ?? EMPTY_NAMES;
}

export function useActiveDiffSessions(): ReadonlySet<string> {
  const q = useQuery(tmuxSessionsQuery());
  return useMemo(() => {
    const list = q.data?.diff;
    if (!list || list.length === 0) return EMPTY;
    return new Set(list);
  }, [q.data]);
}

export function useActiveShellSessions(): ReadonlySet<string> {
  const q = useQuery(tmuxSessionsQuery());
  return useMemo(() => {
    const list = q.data?.shell;
    if (!list || list.length === 0) return EMPTY;
    return new Set(list);
  }, [q.data]);
}
