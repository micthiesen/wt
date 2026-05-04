import { useEffect, useMemo, useState } from "react";
import { useSyncExternalStore } from "react";

import {
  RECENT_WINDOW_MS,
  actionRegistry,
  type ActionRun,
} from "../../core/actions.ts";

/**
 * The current run for a slug, or `null` when there is none. Re-renders
 * any time the registry mutates — the snapshot is the registry's
 * `Map<slug, ActionRun>`, identity changes on every update.
 */
export function useAction(slug: string | undefined): ActionRun | null {
  const map = useSyncExternalStore(
    actionRegistry.subscribe,
    actionRegistry.getSnapshot,
    actionRegistry.getSnapshot,
  );
  if (!slug) return null;
  return map.get(slug) ?? null;
}

/**
 * Set of slugs whose action is *currently* running (not the recent
 * window — that's handled separately by `useActionVisible`). Drives
 * the per-row glyph in the worktree list. Membership is computed from
 * the registry snapshot inside `useMemo`, which re-runs whenever the
 * registry mutates; every run-line append produces a new map identity,
 * but consumers only care about which slugs are running, not how many
 * lines they've emitted.
 */
export function useActiveActions(): ReadonlySet<string> {
  const map = useSyncExternalStore(
    actionRegistry.subscribe,
    actionRegistry.getSnapshot,
    actionRegistry.getSnapshot,
  );
  return useMemo(() => {
    const out = new Set<string>();
    for (const [slug, run] of map) {
      if (run.status === "running") out.add(slug);
    }
    return out;
  }, [map]);
}

/**
 * Whether a finished run for `slug` is still inside the 30-minute
 * "recent" window — drives the activity-pane swap. Returns true while
 * the run is running, true for the 30 minutes after exit, false
 * thereafter. Re-evaluates on every registry mutation AND on a
 * dedicated timer that fires once at the window expiry, so the swap
 * unmounts at the right moment without polling every render.
 */
export function useActionVisible(slug: string | undefined): boolean {
  const run = useAction(slug);
  // `tick` forces a re-render at the window boundary.
  const [, setTick] = useState(0);
  useEffect(() => {
    if (!run || run.status === "running" || run.endedAt === undefined) return;
    const remaining = RECENT_WINDOW_MS - (Date.now() - run.endedAt);
    if (remaining <= 0) return;
    const timer = setTimeout(() => setTick((n) => n + 1), remaining + 50);
    return () => clearTimeout(timer);
  }, [run?.slug, run?.status, run?.endedAt]);
  if (!run) return false;
  if (run.status === "running") return true;
  return run.endedAt !== undefined && Date.now() - run.endedAt < RECENT_WINDOW_MS;
}
