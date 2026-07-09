/**
 * Per-worktree, multi-harness session discovery. Combines:
 *   - Per-harness `discoverSessions` queries (one per impl in
 *     `HARNESSES`), each cached by `(harnessId, slug)`. Liveness is
 *     NOT baked into the cached value — we re-annotate against the
 *     live tmux name set so a tmux flip doesn't invalidate the
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
import type { DerivedState } from "../../core/harness/status.ts";
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
 * because they all funnel through here. `nowMs` only affects the synthetic
 * placeholder's recency.
 *
 * `harnessIds` scopes which harnesses are considered at all (default:
 * every registered harness). This must match the set the caller actually
 * ran discovery for: a harness with an undiscovered-but-empty raw list
 * whose tmux slot is alive would otherwise synthesize a live "(fresh)"
 * placeholder stamped `lastActiveMs: nowMs` — which permanently
 * out-sorts every real session and pins `f12Target` (and the glyph
 * tint) to a placeholder that never changes state.
 */
export function computeHarnessSessions(
  rawByHarness: ReadonlyMap<HarnessId, ReadonlyArray<HarnessSession>>,
  tmuxNames: ReadonlySet<string>,
  slug: string,
  primary: HarnessId,
  nowMs: number,
  harnessIds?: readonly HarnessId[],
): UseHarnessSessionsResult {
  const all: HarnessSessionEntry[] = [];
  for (const h of HARNESSES) {
    if (harnessIds && !harnessIds.includes(h.id)) continue;
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
    const isSingleSlot = h.singleSlot;
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
      // keeps that guess, falling back to `waiting` when no tail/DB message
      // exists yet; a dead slot reads as `idle` unless it died mid-turn.
      // That matches Claude's semantics: closed cleanly is quiet, closed
      // while work was in flight is the only red "abandoned" state.
      let extras = s.extras;
      if (isSingleSlot) {
        if (!isLive) {
          extras = {
            ...extras,
            derivedState:
              extras.derivedState === "working" ? "abandoned" : "idle",
          };
        } else if (extras.derivedState === null) {
          extras = { ...extras, derivedState: "waiting" };
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
  // One discovery query per harness, in registry order. `useQueries`
  // keeps the call count stable (HARNESSES is a fixed constant) and
  // mirrors `useActiveSessionsBySlug` so the two hooks can't drift. The
  // query factory short-circuits to `enabled: false` when wtPath is "".
  // `combine` is load-bearing: the raw `useQueries` results array is a
  // fresh reference every render, so a useMemo keyed on it never holds —
  // the combined data array IS structurally shared by TanStack, making
  // the memos below real.
  const rawData = useQueries({
    queries: HARNESSES.map((h) => harnessSessionsQuery(h.id, slug, wtPath)),
    combine: combineSessionData,
  });
  const rawByHarness = useMemo(() => {
    const m = new Map<HarnessId, ReadonlyArray<HarnessSession>>();
    HARNESSES.forEach((h, i) => m.set(h.id, rawData[i] ?? EMPTY));
    return m;
  }, [rawData]);

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

const ALL_HARNESS_IDS: readonly HarnessId[] = HARNESSES.map((h) => h.id);

/**
 * Shared `useQueries` combiner: project each result down to its data.
 * TanStack structurally shares the combined value, so the returned array
 * keeps a stable identity across renders while no query data changed —
 * which is what lets the downstream `useMemo`s actually memoize.
 */
function combineSessionData(
  results: ReadonlyArray<{ data?: ReadonlyArray<HarnessSession> }>,
): ReadonlyArray<ReadonlyArray<HarnessSession> | undefined> {
  return results.map((r) => r.data);
}

/**
 * Per-slug {@link ActiveSessionGlyph}, computed through the same
 * {@link computeHarnessSessions} rule the current-row hook and F12 use —
 * so the glyph can never disagree with what gets attached or what the
 * details pane shows.
 *
 * `targetHarness` restricts which harness counts. Omit it (list rows) for
 * the cross-harness F12 target — what F12 on a worktree row attaches to.
 * Pass it (footer slots) to track *that* harness's session only, since a
 * slot keybind always opens the TAB-selected primary, not whatever's most
 * recently active — so the footer color follows the same harness as the
 * glyph instead of drifting to a different live session in the slot.
 *
 * The map only ever holds slugs with a *live* session, and liveness is
 * fully determined by the cheap `tmuxSessionsQuery` set, so we fan the
 * (per-harness, disk-scanning) `harnessSessionsQuery` only across slugs
 * tmux already reports as live for the considered harness(es). Discovery
 * cost scales with live sessions, not total worktrees.
 */
export function useActiveSessionsBySlug(
  worktrees: ReadonlyArray<{ slug: string; path: string }>,
  primary: HarnessId,
  targetHarness?: HarnessId,
): ReadonlyMap<string, ActiveSessionGlyph> {
  const tmux = useQuery(tmuxSessionsQuery());
  const harnessIds = useMemo(
    () => (targetHarness ? [targetHarness] : ALL_HARNESS_IDS),
    [targetHarness],
  );
  const slugsByHarness = tmux.data?.slugsByHarness;
  const liveAiSlugs = useMemo(() => {
    const s = new Set<string>();
    for (const id of harnessIds) {
      for (const slug of slugsByHarness?.[id] ?? []) s.add(slug);
    }
    return s;
  }, [slugsByHarness, harnessIds]);
  const liveWorktrees = useMemo(
    () => worktrees.filter((w) => liveAiSlugs.has(w.slug)),
    [worktrees, liveAiSlugs],
  );
  // `combine` for the same reason as `useHarnessSessions`: the raw
  // results array identity changes every render; the combined data
  // array is structurally shared, so the memo below actually holds.
  const rawData = useQueries({
    queries: liveWorktrees.flatMap((w) =>
      harnessIds.map((id) => harnessSessionsQuery(id, w.slug, w.path)),
    ),
    combine: combineSessionData,
  });
  return useMemo(() => {
    const tmuxNames = new Set(tmux.data?.all ?? []);
    const now = Date.now();
    const H = harnessIds.length;
    const map = new Map<string, ActiveSessionGlyph>();
    for (let i = 0; i < liveWorktrees.length; i++) {
      const w = liveWorktrees[i]!;
      const rawByHarness = new Map<HarnessId, ReadonlyArray<HarnessSession>>();
      for (let j = 0; j < H; j++) {
        rawByHarness.set(harnessIds[j]!, rawData[i * H + j] ?? EMPTY);
      }
      const { f12Target } = computeHarnessSessions(
        rawByHarness,
        tmuxNames,
        w.slug,
        primary,
        now,
        // Scope to the harnesses we actually ran discovery for. Without
        // this, a `targetHarness`-restricted call (footer slots) sees an
        // empty raw list for the OTHER harnesses while their tmux slot
        // may be alive (e.g. an idle `wt-codex` next to the claude `wt`
        // session) and pins f12Target to a synthetic placeholder.
        harnessIds,
      );
      if (f12Target?.isLive) {
        map.set(w.slug, {
          harnessId: f12Target.harnessId,
          state: f12Target.extras.derivedState,
        });
      }
    }
    return map;
  }, [rawData, tmux.data?.all, primary, liveWorktrees, harnessIds]);
}
