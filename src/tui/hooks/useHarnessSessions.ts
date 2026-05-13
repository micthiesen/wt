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
  getHarness,
  HARNESSES,
  type HarnessId,
  type HarnessSession,
} from "../../core/harness/index.ts";
import {
  harnessSessionsQuery,
  tmuxSessionsQuery,
} from "../../state/queries.ts";

export type HarnessSessionEntry = HarnessSession & { harnessId: HarnessId };

/**
 * Sentinel sessionId prefix used by the synthesized live-slot placeholder
 * for codex/opencode when the tmux slot is alive but no on-disk session
 * record exists yet. `commitRow` strips this and passes
 * `resumeSessionId: null` so the spawn just attaches to the slot.
 */
export const SYNTHETIC_LIVE_PREFIX = "__live__";

export function isSyntheticLiveSessionId(sessionId: string): boolean {
  return sessionId.startsWith(SYNTHETIC_LIVE_PREFIX);
}

function mostRecentSessionId(
  raw: ReadonlyArray<HarnessSession>,
): string | null {
  let best: HarnessSession | null = null;
  for (const s of raw) {
    if (!best || (s.lastActiveMs ?? 0) > (best.lastActiveMs ?? 0)) {
      best = s;
    }
  }
  return best?.sessionId ?? null;
}

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
      // Single-tmux-per-slug for codex/opencode means at most ONE
      // discovered session can actually be running in the slot at any
      // time. The previous "any session whose tmuxSessionName matches a
      // live tmux name is live" rule marked EVERY discovered session
      // live whenever the slot was alive, which is wrong and made
      // resume-vs-attach indistinguishable in the picker. Resolve it
      // here: when the slot is alive, the most-recently-active
      // discovered session represents the slot; all others are dead.
      // When the slot is alive but no discovered session points at it
      // yet (fresh codex/opencode before the first prompt — the only
      // moment when rollout/DB write hasn't happened), synthesize a
      // placeholder so the picker isn't blank for an actively-running
      // session.
      const isSingleSlot = h.id === "codex" || h.id === "opencode";
      // Pull the tmux name from the canonical harness impl rather than
      // re-deriving `${slug}-${h.id}` inline — keeps this hook in sync
      // if the format ever changes (e.g. a future multi-slot scheme).
      const slotTmuxName = isSingleSlot
        ? getHarness(h.id).tmuxSessionName(slug, null)
        : null;
      const slotAlive = slotTmuxName !== null && tmuxNames.has(slotTmuxName);
      const liveDiscoveredId =
        isSingleSlot && slotAlive
          ? mostRecentSessionId(raw)
          : null;
      let annotated: HarnessSessionEntry[] = raw.map((s) => {
        const isLive = isSingleSlot
          ? s.sessionId === liveDiscoveredId
          : tmuxNames.has(s.tmuxSessionName);
        // Finalize opencode/codex derived state now that we know liveness.
        // discoverSessions() returns state from DB/tail without liveness;
        // we apply the liveness-dependent transitions here.
        let extras = s.extras;
        if (isSingleSlot && extras.derivedState !== null) {
          const st = extras.derivedState;
          let finalState = st;
          if (isLive && (st === "idle" || st === "abandoned")) {
            // Tmux is live; session is active even if DB shows idle.
            finalState = "waiting";
          } else if (!isLive && st === "waiting") {
            // Not live. Fresh tail (< 10 min) → abandoned; old → idle.
            const tailMs = extras.tailEndedAt ?? 0;
            const ageMs = Date.now() - tailMs;
            finalState = ageMs < 10 * 60 * 1000 ? "abandoned" : "idle";
          }
          if (finalState !== st) {
            extras = { ...extras, derivedState: finalState };
          }
        }
        return { ...s, isLive, harnessId: h.id, extras };
      });
      if (isSingleSlot && slotAlive && liveDiscoveredId === null) {
        // Slot is alive but nothing on disk points at it yet — codex
        // and opencode don't persist a rollout/DB row until the first
        // user prompt, so a freshly spawned session is invisible to
        // discovery. Surface a placeholder so the user can re-attach
        // (or kill) it from the picker. Sentinel sessionId is consumed
        // by commitRow → enterHarnessSession to mean "attach to the
        // live slot, no resume id". `lastActiveMs: Date.now()` floats
        // it to the top of the sorted list.
        annotated = [
          {
            displayName: `(fresh ${h.label})`,
            sessionId: `${SYNTHETIC_LIVE_PREFIX}${h.id}:${slug}`,
            tmuxSessionName: slotTmuxName!,
            lastActiveMs: Date.now(),
            isLive: true,
            harnessId: h.id,
            extras: {
              managedName: null,
              derivedState: "waiting",
              queued: 0,
              tailEndedAt: null,
            },
          },
          ...annotated,
        ];
      }
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
