/**
 * Hooks returning the sets of slugs that currently own live wt tmux
 * sessions, per kind. The claude variants
 * (`useClaudeSessionsBySlug` / `useClaudeSessionsForSlug`) drive the
 * worktree list count badge, the details-pane claude row's state
 * derivation, and the sessions picker. `useActiveDiffSessions` /
 * `useActiveShellSessions` power the Shift+F11 / Shift+F10
 * kill-confirm hints. One global query powers all of them — see
 * `tmuxSessionsQuery` in `state/queries.ts`.
 *
 * `useClaudeSessionsBySlug` exposes the multi-session shape: a Map
 * from slug to the list of session names live on it (`null` is the
 * primary). `useClaudeSessionsForSlug` is the per-row convenience.
 */
import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";

import type { TailHarnessId } from "../../core/harness/harness-tail.ts";
import { tmuxSessionsQuery } from "../../state/queries.ts";

const EMPTY: ReadonlySet<string> = new Set();
const EMPTY_NAMES: ReadonlyArray<string | null> = [];
const EMPTY_MAP: ReadonlyMap<string, ReadonlyArray<string | null>> = new Map();

/**
 * Map from slug → list of live claude session names on that slug.
 * `null` entry = primary session. Entries are deduped and stable in
 * tmux-listed order. Empty slugs are absent (no key) rather than
 * present-with-empty-array, so consumers can `.get(slug) ?? []`.
 *
 * Depends on the inner `claude` array (not the wrapping `q.data`)
 * so two refetches with identical contents reuse the prior Map
 * identity — downstream effects/memos don't re-fire.
 */
export function useClaudeSessionsBySlug(): ReadonlyMap<
  string,
  ReadonlyArray<string | null>
> {
  const q = useQuery(tmuxSessionsQuery());
  const list = q.data?.claude;
  return useMemo(() => {
    if (!list || list.length === 0) return EMPTY_MAP;
    const map = new Map<string, (string | null)[]>();
    for (const entry of list) {
      const arr = map.get(entry.slug);
      if (arr) arr.push(entry.name);
      else map.set(entry.slug, [entry.name]);
    }
    return map;
  }, [list]);
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
  const list = q.data?.diff;
  return useMemo(() => {
    if (!list || list.length === 0) return EMPTY;
    return new Set(list);
  }, [list]);
}

export function useActiveShellSessions(): ReadonlySet<string> {
  const q = useQuery(tmuxSessionsQuery());
  const list = q.data?.shell;
  return useMemo(() => {
    if (!list || list.length === 0) return EMPTY;
    return new Set(list);
  }, [list]);
}

/** Slugs with a live codex/opencode tmux slot. Drives the harness-tail
 *  reconcile so the bottom pane tails only live sessions. */
export function useActiveHarnessSessions(
  harnessId: TailHarnessId,
): ReadonlySet<string> {
  const q = useQuery(tmuxSessionsQuery());
  const list = q.data?.slugsByHarness[harnessId];
  return useMemo(() => {
    if (!list || list.length === 0) return EMPTY;
    return new Set(list);
  }, [list]);
}
