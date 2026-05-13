/**
 * Per-worktree, multi-harness session discovery. Combines:
 *   - Per-harness `discoverSessions` queries (one per impl in
 *     `HARNESSES`), each cached by `(harnessId, slug)`. Liveness is
 *     NOT baked into the cached value — we re-annotate against the
 *     live tmux name set so a 2s tmux flip doesn't invalidate the
 *     potentially-slow discovery cache (sqlite query, rollout scan).
 *   - The tmux name set from `tmuxSessionsQuery` for liveness.
 *
 * Output shape:
 *   - `sessions` is every session known across every harness, each
 *     tagged with its harness id, sorted most-recently-active first.
 *   - `f12Target` is the session F12 would attach to right now: the
 *     most-recently-active session that's currently live, or the
 *     primary harness's most-recently-active dead session, or null
 *     when nothing exists for any harness.
 *   - `byHarness` indexes the same data per id for picker entries.
 */
import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";

import {
  HARNESSES,
  type HarnessId,
  type HarnessSession,
} from "../../core/harness/index.ts";
import {
  harnessSessionsQuery,
  tmuxSessionsQuery,
} from "../../state/queries.ts";

export type HarnessSessionEntry = HarnessSession & { harnessId: HarnessId };

export type UseHarnessSessionsResult = {
  sessions: ReadonlyArray<HarnessSessionEntry>;
  byHarness: ReadonlyMap<HarnessId, ReadonlyArray<HarnessSessionEntry>>;
  /**
   * Most-recently-active session that's currently live (across any
   * harness). When nothing is live, the primary's most-recently-
   * active dead session. When no sessions exist anywhere, null.
   * F12 attaches to this; if null, F12 spawns the primary fresh.
   */
  f12Target: HarnessSessionEntry | null;
};

const EMPTY: HarnessSession[] = [];

export function useHarnessSessions(
  slug: string,
  wtPath: string,
  primary: HarnessId,
): UseHarnessSessionsResult {
  const tmux = useQuery(tmuxSessionsQuery());
  // Hooks must be called unconditionally so we always invoke one per
  // harness in registry order. The query factory short-circuits to
  // `enabled: false` when wtPath is empty.
  const claudeQ = useQuery(harnessSessionsQuery("claude", slug, wtPath));
  const codexQ = useQuery(harnessSessionsQuery("codex", slug, wtPath));
  const opencodeQ = useQuery(harnessSessionsQuery("opencode", slug, wtPath));
  const queries = useMemo(() => {
    return new Map<HarnessId, HarnessSession[]>([
      ["claude", claudeQ.data ?? EMPTY],
      ["codex", codexQ.data ?? EMPTY],
      ["opencode", opencodeQ.data ?? EMPTY],
    ]);
  }, [claudeQ.data, codexQ.data, opencodeQ.data]);

  return useMemo(() => {
    const tmuxNames = new Set(tmux.data?.all ?? []);
    const byHarness = new Map<HarnessId, HarnessSessionEntry[]>();
    const all: HarnessSessionEntry[] = [];
    for (const h of HARNESSES) {
      const raw = queries.get(h.id) ?? EMPTY;
      const annotated: HarnessSessionEntry[] = raw.map((s) => ({
        ...s,
        isLive: tmuxNames.has(s.tmuxSessionName),
        harnessId: h.id,
      }));
      byHarness.set(h.id, annotated);
      all.push(...annotated);
    }
    all.sort((a, b) => (b.lastActiveMs ?? 0) - (a.lastActiveMs ?? 0));
    // F12 target: prefer a live session; if none live, fall back to
    // the most-recently-active session in the primary harness (so the
    // hint shown in the AI row reflects what F12 will spawn).
    let f12Target: HarnessSessionEntry | null = null;
    for (const e of all) {
      if (e.isLive) {
        f12Target = e;
        break;
      }
    }
    if (!f12Target) {
      const primaryEntries = byHarness.get(primary) ?? [];
      f12Target = primaryEntries[0] ?? null;
    }
    return { sessions: all, byHarness, f12Target };
  }, [queries, tmux.data?.all, primary]);
}
