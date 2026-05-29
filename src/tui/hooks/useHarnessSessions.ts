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
 *     tagged with its harness id, sorted by `compareSessionsForDisplay`
 *     (live first, then most-recently-active) — the order both session
 *     pickers consume directly.
 *   - `f12Target` is the session F12 would attach to right now: the
 *     most-recently-active session that's currently live, or the
 *     primary harness's most-recently-active dead session, or null
 *     when nothing exists for any harness.
 */
import { useMemo } from "react";
import { useQueries, useQuery } from "@tanstack/react-query";

import {
  getHarness,
  HARNESSES,
  type HarnessId,
  type HarnessSession,
} from "../../core/harness/index.ts";
import type { DerivedState } from "../../core/claude-status.ts";
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

/**
 * Canonical display order for a session list: live sessions first, then
 * most-recently-active within each bucket. The single comparator both
 * session pickers and the `f12Target` derivation rely on, so the "what's
 * active" ordering can't drift between them.
 */
export function compareSessionsForDisplay(
  a: HarnessSessionEntry,
  b: HarnessSessionEntry,
): number {
  if (a.isLive !== b.isLive) return a.isLive ? -1 : 1;
  return (b.lastActiveMs ?? 0) - (a.lastActiveMs ?? 0);
}

export type UseHarnessSessionsResult = {
  sessions: ReadonlyArray<HarnessSessionEntry>;
  /**
   * Most-recently-active session that's currently live (across any
   * harness). When nothing is live, the primary's most-recently-
   * active dead session. When no sessions exist anywhere, null.
   * F12 attaches to this; if null, F12 spawns the primary fresh.
   */
  f12Target: HarnessSessionEntry | null;
};

const EMPTY: readonly HarnessSession[] = [];

/**
 * The single source of truth for "which session is active" — turning raw
 * per-harness discovery + the live tmux name set into the sorted session
 * list and the F12 target. Pure (time injected via `nowMs`) so it runs
 * identically whether driven by the per-current-row {@link
 * useHarnessSessions} or the all-rows {@link useActiveSessionsBySlug};
 * neither the list pane, the details pane, nor the F12 keybind can drift
 * because they all funnel through here. `nowMs` only affects the
 * codex/opencode idle-vs-abandoned age cutoff and the synthetic
 * placeholder's recency.
 */
export function computeHarnessSessions(
  rawByHarness: ReadonlyMap<HarnessId, ReadonlyArray<HarnessSession>>,
  tmuxNames: ReadonlySet<string>,
  slug: string,
  primary: HarnessId,
  nowMs: number,
): UseHarnessSessionsResult {
  const all: HarnessSessionEntry[] = [];
  for (const h of HARNESSES) {
    const raw = rawByHarness.get(h.id) ?? EMPTY;
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
      isSingleSlot && slotAlive ? mostRecentSessionId(raw) : null;
    let annotated: HarnessSessionEntry[] = raw.map((s) => {
      const isLive = isSingleSlot
        ? s.sessionId === liveDiscoveredId
        : tmuxNames.has(s.tmuxSessionName);
      // Finalize codex/opencode derived state now that we know liveness.
      // discoverSessions() returns a liveness-independent best guess
      // (working = mid-turn/streaming, waiting = turn closed). A live slot
      // keeps that guess; a dead slot is abandoned (recent) or idle (old),
      // by the last-activity age — so a working session reads `working`
      // rather than being floored to `waiting`.
      let extras = s.extras;
      if (isSingleSlot && extras.derivedState !== null) {
        if (!isLive) {
          const tailMs = extras.tailEndedAt ?? 0;
          const ageMs = nowMs - tailMs;
          extras = {
            ...extras,
            derivedState: ageMs < 10 * 60 * 1000 ? "abandoned" : "idle",
          };
        }
      } else if (!isSingleSlot && isLive) {
        // Claude: discoverSessions derives state with isTmuxLive=false
        // (liveness isn't known there). A live session whose registry
        // entry is missing — write failed, or a pre-2.1 build — lands on
        // a not-live state: "abandoned" (midTurn) or "idle" (clean tail).
        // It's actually live, so mirror deriveSessionState's live branch
        // — midTurn → working, otherwise → waiting (your move). Without
        // this, "idle" would tint the glyph dim like a dead session.
        if (extras.derivedState === "abandoned") {
          extras = { ...extras, derivedState: "working" };
        } else if (extras.derivedState === "idle") {
          extras = { ...extras, derivedState: "waiting" };
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
      // live slot, no resume id". `lastActiveMs: nowMs` floats it to
      // the top of the sorted list.
      annotated = [
        {
          displayName: `(fresh ${h.label})`,
          sessionId: `${SYNTHETIC_LIVE_PREFIX}${h.id}:${slug}`,
          tmuxSessionName: slotTmuxName!,
          lastActiveMs: nowMs,
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
    all.push(...annotated);
  }
  all.sort(compareSessionsForDisplay);
  // F12 target: prefer a live session; if none live, fall back to the
  // most-recently-active session in the primary harness (so the hint
  // shown in the AI row reflects what F12 will spawn). `all` is sorted
  // live-first then recency-desc, so the first live entry is the most
  // recent live one, and `find` over the primary harness yields its
  // most-recently-active (here necessarily dead) session.
  let f12Target: HarnessSessionEntry | null = null;
  for (const e of all) {
    if (e.isLive) {
      f12Target = e;
      break;
    }
  }
  if (!f12Target) {
    f12Target = all.find((e) => e.harnessId === primary) ?? null;
  }
  return { sessions: all, f12Target };
}

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
  const rawByHarness = useMemo(() => {
    return new Map<HarnessId, ReadonlyArray<HarnessSession>>([
      ["claude", claudeQ.data ?? EMPTY],
      ["codex", codexQ.data ?? EMPTY],
      ["opencode", opencodeQ.data ?? EMPTY],
    ]);
  }, [claudeQ.data, codexQ.data, opencodeQ.data]);

  return useMemo(
    () =>
      computeHarnessSessions(
        rawByHarness,
        new Set(tmux.data?.all ?? []),
        slug,
        primary,
        Date.now(),
      ),
    [rawByHarness, tmux.data?.all, slug, primary],
  );
}

/**
 * Distilled "active session" for the list pane's harness glyph: the
 * harness F12 would attach to right now, plus its derived state for
 * tinting. `null`/absent when no session is *live* — the list reads as
 * live activity, so a dead F12 fallback (resume-on-press) isn't shown.
 */
export type ActiveSessionGlyph = {
  harnessId: HarnessId;
  /** Cross-harness derived state, or null (live codex/opencode w/o one). */
  state: DerivedState | null;
};

/**
 * Per-slug {@link ActiveSessionGlyph}, computed through the same
 * {@link computeHarnessSessions} rule the current-row hook and F12 use —
 * so the list glyph can never disagree with what F12 attaches to or what
 * the details pane shows.
 *
 * The map only ever holds slugs with a *live* session, and liveness is
 * fully determined by the cheap `tmuxSessionsQuery` set, so we fan the
 * (per-harness, disk-scanning) `harnessSessionsQuery` only across slugs
 * tmux already reports as having a live AI session rather than every
 * worktree. Output is identical; the discovery cost scales with live
 * sessions, not total worktrees.
 */
export function useActiveSessionsBySlug(
  worktrees: ReadonlyArray<{ slug: string; path: string }>,
  primary: HarnessId,
): ReadonlyMap<string, ActiveSessionGlyph> {
  const tmux = useQuery(tmuxSessionsQuery());
  const liveAiSlugs = useMemo(() => {
    const s = new Set<string>();
    for (const slug of tmux.data?.claudeSlugs ?? []) s.add(slug);
    for (const slug of tmux.data?.codex ?? []) s.add(slug);
    for (const slug of tmux.data?.opencode ?? []) s.add(slug);
    return s;
  }, [tmux.data?.claudeSlugs, tmux.data?.codex, tmux.data?.opencode]);
  const liveWorktrees = useMemo(
    () => worktrees.filter((w) => liveAiSlugs.has(w.slug)),
    [worktrees, liveAiSlugs],
  );
  const results = useQueries({
    queries: liveWorktrees.flatMap((w) =>
      HARNESSES.map((h) => harnessSessionsQuery(h.id, w.slug, w.path)),
    ),
  });
  return useMemo(() => {
    const tmuxNames = new Set(tmux.data?.all ?? []);
    const now = Date.now();
    const H = HARNESSES.length;
    const map = new Map<string, ActiveSessionGlyph>();
    for (let i = 0; i < liveWorktrees.length; i++) {
      const w = liveWorktrees[i]!;
      const rawByHarness = new Map<HarnessId, ReadonlyArray<HarnessSession>>();
      for (let j = 0; j < H; j++) {
        rawByHarness.set(HARNESSES[j]!.id, results[i * H + j]?.data ?? EMPTY);
      }
      const { f12Target } = computeHarnessSessions(
        rawByHarness,
        tmuxNames,
        w.slug,
        primary,
        now,
      );
      if (f12Target?.isLive) {
        map.set(w.slug, {
          harnessId: f12Target.harnessId,
          state: f12Target.extras.derivedState,
        });
      }
    }
    return map;
  }, [results, tmux.data?.all, primary, liveWorktrees]);
}
