import { readWtState, withWtStateLock, writeWtState } from "./io.ts";
import { GROUP_INBOX, stackIdFromSectionKey } from "./types.ts";
import type { WtSlugState, WtState } from "./types.ts";

/**
 * Drop dead groups from `sectionsOrder`: manual sections no slug
 * references. The inbox sentinel always survives (it's the migration
 * marker and the inbox is never deletable), and stack keys are kept
 * unconditionally — stacks are inferred from the live worktree list,
 * which this module doesn't see, so a stale key can't be told from a
 * momentarily-empty one. Stale stack keys are inert and tiny.
 */
function prunedSectionsOrder(state: WtState): string[] {
  const live = new Set<string>();
  for (const v of Object.values(state.slugs)) {
    if (v.section !== null) live.add(v.section);
  }
  return state.sectionsOrder.filter((s) => {
    if (s === GROUP_INBOX) return true;
    if (stackIdFromSectionKey(s) !== null) return true;
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
 * Advance a slug's replay anchor, but only while its recorded parent
 * still matches what the replay resolved at chain time. A concurrent
 * writer — typically a background destroy reparenting this slug off a
 * just-deleted parent — wins: clobbering its rewrite with the stale
 * pre-destroy parent would leave the record pointing at a dead branch.
 * Returns false when the record moved and the anchor was left alone
 * (still valid — it names a tip that's an ancestor of the branch either
 * way; the next reconcile/replay recomputes from it).
 */
export function advanceBaseAnchor(
  slug: string,
  expectedParent: string,
  sha: string,
): boolean {
  return withWtStateLock(() => {
    const state = readWtState();
    const prev = state.slugs[slug];
    if (prev?.baseBranch !== expectedParent) return false;
    const next: WtState = { ...state, slugs: { ...state.slugs } };
    next.slugs[slug] = { ...prev, baseSha: sha };
    writeWtState(next);
    return true;
  });
}

/**
 * Reparent every slug's recorded fork base that points at `branch` onto
 * the deleted branch's OWN recorded base (falling back to `trunk`).
 * Called by destroy after the branch is deleted — a dangling record
 * would keep rendering the stack against a ref that no longer resolves.
 * Each dependent's `baseSha` anchor is PRESERVED: it still names the
 * tip the dependent's own commits sit on, which is what keeps the next
 * replay squash-safe after the parent landed. Returns the affected
 * slugs for logging.
 *
 * When `deletedSlug` is given, the deleted branch's own record (still
 * in the state file — per-slug entries are reaped later, at startup) is
 * consulted for the new parent, so a mid-chain removal re-links
 * grandchildren to their grandparent instead of flattening them to
 * trunk.
 */
export function reparentBaseReferences(
  branch: string,
  trunk: string,
  deletedSlug?: string,
): string[] {
  return withWtStateLock(() => {
    const state = readWtState();
    const affected = Object.entries(state.slugs)
      .filter(([slug, s]) => s.baseBranch === branch && slug !== deletedSlug)
      .map(([slug]) => slug);
    if (affected.length === 0) return affected;
    const deletedBase = deletedSlug ? state.slugs[deletedSlug]?.baseBranch : undefined;
    const newParent = deletedBase && deletedBase !== branch ? deletedBase : trunk;
    const next: WtState = { ...state, slugs: { ...state.slugs } };
    for (const slug of affected) {
      const entry: WtSlugState = { ...next.slugs[slug]! };
      entry.baseBranch = newParent;
      // baseSha intentionally kept — the squash-safe anchor.
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
 * Insert any inferred-stack key that's in `visualOrder` but not yet in
 * `order` at its real on-screen slot: right after its nearest preceding
 * visual group that IS already ranked (front if none). Inferred stacks
 * never pre-register in `sectionsOrder` (an unranked one sorts to the
 * top), so a reorder that names one as a landmark would otherwise have
 * no anchor. Seeding them at their visual position — rather than blindly
 * at the front — keeps a move relative to exactly what the user sees,
 * even with several never-moved stacks on screen at once. Only stack
 * keys are seeded; manual names + the inbox are always self-healed into
 * `sectionsOrder` at read time.
 */
function seedVisualStacks(
  order: readonly string[],
  visualOrder: readonly string[],
): string[] {
  const out = [...order];
  for (let i = 0; i < visualOrder.length; i++) {
    const g = visualOrder[i]!;
    if (out.includes(g)) continue;
    if (stackIdFromSectionKey(g) === null) continue;
    let anchor = -1; // -1 → splice at 0 (front)
    for (let j = i - 1; j >= 0; j--) {
      const at = out.indexOf(visualOrder[j]!);
      if (at >= 0) {
        anchor = at;
        break;
      }
    }
    out.splice(anchor + 1, 0, g);
  }
  return out;
}

/**
 * Reorder the group list: remove `key` and reinsert it immediately
 * before/after `pastKey`. Manual keys must already be present (they're
 * self-healed into `sectionsOrder` at read time); an inferred STACK key
 * is registered on demand — seeded at its visual slot via `visualOrder`
 * (the present groups in display order), so moving a never-moved stack
 * section works the same as any manual one. Returns false when a manual
 * key is absent, the keys are equal, or the result is a no-op. "Place
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
  visualOrder: readonly string[] = [],
): boolean {
  return withWtStateLock(() => {
    const state = readWtState();
    if (key === pastKey) return false;
    let order = seedVisualStacks(state.sectionsOrder, visualOrder);
    // Backstop: a stack key named as key/landmark but absent from
    // `visualOrder` (shouldn't happen — the caller derives both from the
    // same rows) still gets registered so the move can't silently fail.
    for (const k of [key, pastKey]) {
      if (!order.includes(k) && stackIdFromSectionKey(k) !== null) {
        order = [k, ...order];
      }
    }
    if (!order.includes(key)) return false;
    const arr = order.filter((s) => s !== key);
    const at = arr.indexOf(pastKey);
    if (at < 0) return false;
    arr.splice(side === "before" ? at : at + 1, 0, key);
    if (arr.every((s, i) => s === state.sectionsOrder[i]) && arr.length === state.sectionsOrder.length) {
      return false;
    }
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
