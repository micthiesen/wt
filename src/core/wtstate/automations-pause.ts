import { readWtState, withWtStateLock, writeWtState } from "./io.ts";
import type { WtSlugState, WtState } from "./types.ts";

/**
 * Toggle the per-worktree automations pause flag. Returns the new
 * paused state. Creates the slug entry on first write (like
 * `setSlugBase`) so a brand-new worktree can be paused before it has
 * any section/order state.
 */
export function toggleSlugAutomationsPaused(slug: string): boolean {
  return withWtStateLock(() => {
    const state = readWtState();
    const prev = state.slugs[slug];
    const next: WtState = { ...state, slugs: { ...state.slugs } };
    const entry: WtSlugState = { section: null, order: 0, ...prev };
    const paused = entry.automationsPaused !== true;
    if (paused) entry.automationsPaused = true;
    else delete entry.automationsPaused;
    next.slugs[slug] = entry;
    writeWtState(next);
    return paused;
  });
}

/**
 * Toggle the whole-stack automations pause (Ctrl+A on a stack member or
 * its folded header). Returns the new paused state. Prunes ids whose
 * manifest is gone while it's writing anyway, so the list can't grow
 * stale entries forever.
 */
export function toggleStackAutomationsPaused(stackId: string): boolean {
  return withWtStateLock(() => {
    const state = readWtState();
    const paused = !state.pausedStacks.includes(stackId);
    const pausedStacks = state.pausedStacks
      .filter((id) => id !== stackId && id in state.stacks)
      .concat(paused ? [stackId] : []);
    writeWtState({ ...state, pausedStacks });
    return paused;
  });
}

/** Toggle the persisted GLOBAL automations pause (Shift+A). Returns the new state. */
export function toggleGlobalAutomationsPaused(): boolean {
  return withWtStateLock(() => {
    const state = readWtState();
    const paused = !state.automationsPaused;
    writeWtState({ ...state, automationsPaused: paused });
    return paused;
  });
}
