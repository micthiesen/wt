import { readWtState, withWtStateLock, writeWtState } from "./io.ts";
import { GROUP_INBOX, stackIdFromSectionKey } from "./types.ts";
import type { WtSlugState, WtState } from "./types.ts";

/**
 * Drop dead groups from `sectionsOrder`: manual sections no slug
 * references and stack keys whose manifest is gone. The inbox sentinel
 * always survives (it's the migration marker and the inbox is never
 * deletable).
 */
function prunedSectionsOrder(state: WtState): string[] {
  const live = new Set<string>();
  for (const v of Object.values(state.slugs)) {
    if (v.section !== null) live.add(v.section);
  }
  return state.sectionsOrder.filter((s) => {
    if (s === GROUP_INBOX) return true;
    const sid = stackIdFromSectionKey(s);
    if (sid !== null) return sid in state.stacks;
    return live.has(s);
  });
}

function ensureSection(state: WtState, section: string): WtState {
  if (state.sectionsOrder.includes(section)) return state;
  return { ...state, sectionsOrder: [...state.sectionsOrder, section] };
}

/** Drop a single slug's entry. No-op if absent. */
export function clearSlugState(slug: string): void {
  withWtStateLock(() => {
    const state = readWtState();
    if (!(slug in state.slugs)) return;
    const next = { ...state, slugs: { ...state.slugs } };
    delete next.slugs[slug];
    next.sectionsOrder = prunedSectionsOrder(next);
    writeWtState(next);
  });
}

/** Max order in a given section. Returns `null` when section is empty. */
function maxOrderIn(state: WtState, section: string | null): number | null {
  let max = -Infinity;
  for (const v of Object.values(state.slugs)) {
    if (v.section === section && v.order > max) max = v.order;
  }
  return Number.isFinite(max) ? max : null;
}

/** Min order in a given section. Returns `null` when section is empty. */
function minOrderIn(state: WtState, section: string | null): number | null {
  let min = Infinity;
  for (const v of Object.values(state.slugs)) {
    if (v.section === section && v.order < min) min = v.order;
  }
  return Number.isFinite(min) ? min : null;
}

/**
 * Place a slug at the top or bottom of a section. "Bottom" = max+1
 * (default for picker / generic assignment). "Top" = min-1, used by
 * Shift+J across a section boundary so the moved row lands adjacent
 * to where it was. Source section's other members keep their orders
 * and `sectionsOrder` is pruned of any section that just emptied.
 */
export function placeSlug(
  slug: string,
  section: string | null,
  position: "top" | "bottom",
): void {
  withWtStateLock(() => {
    let state = readWtState();
    if (section !== null) state = ensureSection(state, section);
    const next: WtState = { ...state, slugs: { ...state.slugs } };
    let order: number;
    if (position === "top") {
      const min = minOrderIn(next, section);
      order = min === null ? 0 : min - 1;
    } else {
      const max = maxOrderIn(next, section);
      order = max === null ? 0 : max + 1;
    }
    next.slugs[slug] = { ...next.slugs[slug], section, order };
    next.sectionsOrder = prunedSectionsOrder(next);
    writeWtState(next);
  });
}

/**
 * Record (or clear, with `base = null`) a worktree's fork base. Creates
 * the slug entry on first write so a brand-new worktree (no manual
 * section/order yet) can still carry its base.
 */
export function setSlugBase(
  slug: string,
  base: { branch: string; sha?: string } | null,
): void {
  withWtStateLock(() => {
    const state = readWtState();
    const prev = state.slugs[slug];
    if (!prev && !base) return;
    const next: WtState = { ...state, slugs: { ...state.slugs } };
    const entry: WtSlugState = { section: null, order: 0, ...prev };
    delete entry.baseBranch;
    delete entry.baseSha;
    if (base) {
      entry.baseBranch = base.branch;
      if (base.sha) entry.baseSha = base.sha;
    }
    next.slugs[slug] = entry;
    writeWtState(next);
  });
}

/**
 * Drop every slug's recorded fork base that points at `branch`. Called
 * by destroy after the branch is deleted — a dangling record would
 * keep rendering "(forked)" against a ref that no longer resolves
 * (the diff layer degrades to trunk via `effectiveBaseOrTrunk`, but
 * the stale label and sync counts linger). Returns the affected slugs
 * for logging.
 */
export function clearBaseReferences(branch: string): string[] {
  return withWtStateLock(() => {
    const state = readWtState();
    const affected = Object.entries(state.slugs)
      .filter(([, s]) => s.baseBranch === branch)
      .map(([slug]) => slug);
    if (affected.length === 0) return affected;
    const next: WtState = { ...state, slugs: { ...state.slugs } };
    for (const slug of affected) {
      const entry: WtSlugState = { ...next.slugs[slug]! };
      delete entry.baseBranch;
      delete entry.baseSha;
      next.slugs[slug] = entry;
    }
    writeWtState(next);
    return affected;
  });
}

/**
 * Convenience for the common "assign this slug to that section, drop
 * it at the bottom" path used by the picker.
 */
export function setSlugSection(slug: string, section: string | null): void {
  placeSlug(slug, section, "bottom");
}

/**
 * Swap two slugs' order values within a single section bucket.
 * Renormalizes the bucket against `bucketDisplay` first so unstated
 * entries get materialized. The renormalization preserves the
 * section's current min order (anchors the new sequence at that
 * baseline) so section-display position is stable across the swap.
 */
export function swapOrders(
  slugA: string,
  slugB: string,
  section: string | null,
  bucketDisplay: readonly string[],
): void {
  withWtStateLock(() => {
    const state = readWtState();
    const next: WtState = { ...state, slugs: { ...state.slugs } };
    const min = minOrderIn(next, section);
    const baseline = min === null ? 0 : min;
    for (let i = 0; i < bucketDisplay.length; i++) {
      const slug = bucketDisplay[i]!;
      const prev = next.slugs[slug];
      next.slugs[slug] = { ...prev, section, order: baseline + i };
    }
    const a = next.slugs[slugA];
    const b = next.slugs[slugB];
    if (!a || !b) return;
    next.slugs[slugA] = { ...a, order: b.order };
    next.slugs[slugB] = { ...b, order: a.order };
    writeWtState(next);
  });
}

/**
 * Rename a section across every slug that references it, plus the
 * `sectionsOrder` index. No-op if `oldName === newName`, the trimmed
 * `newName` is empty, or no slug references `oldName`.
 *
 * Merge case (`newName` already exists as a different section): the
 * surviving slot is the *existing* `newName` — so renaming "X" to "Y"
 * lands all of X's slugs at the bottom of Y, in their existing
 * relative order, and the merged section keeps Y's display position.
 * Source orders get rewritten to `maxOrderIn(Y) + 1, +2, ...` so the
 * merge is collision-free and the display sequence is well-defined.
 */
export function renameSection(oldName: string, newName: string): void {
  const trimmed = newName.trim();
  if (!trimmed || trimmed === oldName) return;
  withWtStateLock(() => {
    const state = readWtState();
    const referenced = Object.values(state.slugs).some((v) => v.section === oldName);
    if (!referenced && !state.sectionsOrder.includes(oldName)) return;
    const next: WtState = { ...state, slugs: { ...state.slugs } };
    const isMerge =
      trimmed !== oldName &&
      (next.sectionsOrder.includes(trimmed) ||
        Object.values(next.slugs).some((v) => v.section === trimmed));
    if (isMerge) {
      // Source slugs in their current within-source display order
      // (ascending by `order`), so the merge appends them after Y's
      // existing items in a sensible sequence.
      const sourceSlugs = Object.entries(next.slugs)
        .filter(([, v]) => v.section === oldName)
        .sort((a, b) => a[1].order - b[1].order);
      const max = maxOrderIn(next, trimmed);
      let cursor = max === null ? 0 : max + 1;
      for (const [k, v] of sourceSlugs) {
        next.slugs[k] = { ...v, section: trimmed, order: cursor++ };
      }
      // Drop oldName from the index; trimmed already lives there.
      next.sectionsOrder = next.sectionsOrder.filter((s) => s !== oldName);
      // The merged-away key is gone; keep the target's fold state as-is.
      next.foldedSections = next.foldedSections.filter((s) => s !== oldName);
    } else {
      for (const [k, v] of Object.entries(next.slugs)) {
        if (v.section === oldName) next.slugs[k] = { ...v, section: trimmed };
      }
      // Replace oldName with trimmed in-place so the section keeps its
      // display position — and carries its folded state to the new name.
      next.sectionsOrder = next.sectionsOrder.map((s) => (s === oldName ? trimmed : s));
      next.foldedSections = next.foldedSections.map((s) => (s === oldName ? trimmed : s));
    }
    next.sectionsOrder = prunedSectionsOrder(next);
    writeWtState(next);
  });
}

/**
 * Reorder the group list: remove `key` and reinsert it immediately
 * before/after `pastKey`. Both keys must be present (groups are
 * self-healed into `sectionsOrder` at read time); returns false when
 * either is absent, they're equal, or the result is a no-op. "Place
 * past" rather than "swap with array neighbor" so the caller can name
 * the next VISIBLE group as the landmark — an invisible group sitting
 * between (an empty inbox) gets jumped in one keypress instead of
 * producing a phantom no-change move. Member slugs keep their `order`
 * values; only the group's rank moves.
 */
export function moveGroupPast(
  key: string,
  pastKey: string,
  side: "before" | "after",
): boolean {
  return withWtStateLock(() => {
    const state = readWtState();
    if (key === pastKey) return false;
    if (!state.sectionsOrder.includes(key)) return false;
    const arr = state.sectionsOrder.filter((s) => s !== key);
    const at = arr.indexOf(pastKey);
    if (at < 0) return false;
    arr.splice(side === "before" ? at : at + 1, 0, key);
    if (arr.every((s, i) => s === state.sectionsOrder[i])) return false;
    writeWtState({ ...state, sectionsOrder: arr });
    return true;
  });
}

/**
 * Toggle whether a section is folded in the list, persisting it. Returns the
 * new folded state. Keyed by the section's key (a manual name or a stack's
 * synthetic `stackSectionKey`). A key for a since-deleted section is inert (no
 * row matches it, so it renders nothing) and is intentionally left in place
 * rather than reaped — harmless, and reaping risks dropping a fold while its
 * rows are momentarily absent during a refresh. `renameSection` does migrate
 * a manual key so a rename doesn't silently unfold.
 */
export function toggleSectionFolded(sectionKey: string): boolean {
  return withWtStateLock(() => {
    const state = readWtState();
    const folded = state.foldedSections.includes(sectionKey);
    const next: WtState = {
      ...state,
      foldedSections: folded
        ? state.foldedSections.filter((s) => s !== sectionKey)
        : [...state.foldedSections, sectionKey],
    };
    writeWtState(next);
    return !folded;
  });
}

/**
 * Reap stale slug entries against the live slug set. Called after
 * destroys to keep the state file tidy. No-op when nothing to drop.
 */
export function reapWtState(liveSlugs: ReadonlySet<string>): void {
  withWtStateLock(() => {
    const state = readWtState();
    let changed = false;
    for (const k of Object.keys(state.slugs)) {
      if (!liveSlugs.has(k)) {
        changed = true;
        break;
      }
    }
    if (!changed) return;
    const next: WtState = { ...state, slugs: {} };
    for (const [k, v] of Object.entries(state.slugs)) {
      if (liveSlugs.has(k)) next.slugs[k] = v;
    }
    next.sectionsOrder = prunedSectionsOrder(next);
    writeWtState(next);
  });
}
