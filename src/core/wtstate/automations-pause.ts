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
 * its folded header). `stackId` is the stack's root branch (stacks are
 * inferred, so liveness can't be checked here — a stale id is inert).
 * Returns the new paused state.
 *
 * The pause is ALSO mirrored onto each current member's per-slug flag:
 * the stack key covers members that join later, but it's keyed by the
 * root branch, which dies when the root lands and is cleaned — the
 * per-slug flags are what keep the survivors paused across that
 * re-root. Resuming clears both layers for the current members.
 */
export function toggleStackAutomationsPaused(
  stackId: string,
  memberSlugs: readonly string[] = [],
): boolean {
  return withWtStateLock(() => {
    const state = readWtState();
    const paused = !state.pausedStacks.includes(stackId);
    const pausedStacks = state.pausedStacks
      .filter((id) => id !== stackId)
      .concat(paused ? [stackId] : []);
    const slugs = { ...state.slugs };
    for (const slug of memberSlugs) {
      const entry: WtSlugState = { section: null, order: 0, ...slugs[slug] };
      if (paused) entry.automationsPaused = true;
      else delete entry.automationsPaused;
      slugs[slug] = entry;
    }
    writeWtState({ ...state, pausedStacks, slugs });
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
