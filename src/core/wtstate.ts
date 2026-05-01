import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

import { createLogger } from "./logger.ts";

const STATE_FILE = join(homedir(), ".cache", "wt", "state.json");
const log = createLogger("[wtstate]");

export type WtSlugState = {
  /** Section name. `null` = unsectioned (rendered at top, no header). */
  section: string | null;
  /** Manual ordering scalar within (section, archived) bucket. Lower = earlier. */
  order: number;
};

/**
 * Persisted state for the worktree list:
 *  - `slugs`: per-worktree section + within-section order.
 *  - `sectionsOrder`: explicit display order for named sections.
 *
 * Why an explicit array instead of deriving section position from
 * `min(order)` of members: derived ordering causes a section to leap
 * up or down whenever its first item moves out, which the user noticed
 * as "weird unexpected reordering". Sections still feel ephemeral
 * (auto-appended on first encounter, pruned when no slug references
 * them) — this array is just a sort hint, not user-managed metadata.
 */
export type WtState = {
  slugs: Record<string, WtSlugState>;
  sectionsOrder: string[];
};

export function readWtState(): WtState {
  if (!existsSync(STATE_FILE)) return { slugs: {}, sectionsOrder: [] };
  try {
    const raw = readFileSync(STATE_FILE, "utf8");
    const data = JSON.parse(raw) as Partial<WtState>;
    const slugs: Record<string, WtSlugState> = {};
    if (data?.slugs && typeof data.slugs === "object") {
      for (const [k, v] of Object.entries(data.slugs)) {
        if (!v || typeof v !== "object") continue;
        const rec = v as Partial<WtSlugState>;
        const section = typeof rec.section === "string" && rec.section.trim() !== ""
          ? rec.section
          : null;
        const order = typeof rec.order === "number" && Number.isFinite(rec.order) ? rec.order : 0;
        slugs[k] = { section, order };
      }
    }
    const sectionsOrder: string[] = [];
    if (Array.isArray(data?.sectionsOrder)) {
      const seen = new Set<string>();
      for (const s of data.sectionsOrder) {
        if (typeof s !== "string" || s.trim() === "") continue;
        if (seen.has(s)) continue;
        seen.add(s);
        sectionsOrder.push(s);
      }
    }
    // Self-heal: any section referenced by a slug but missing from
    // sectionsOrder gets appended in slug-discovery order. Keeps the
    // file consistent even after a hand-edit.
    const known = new Set(sectionsOrder);
    for (const v of Object.values(slugs)) {
      if (v.section !== null && !known.has(v.section)) {
        sectionsOrder.push(v.section);
        known.add(v.section);
      }
    }
    return { slugs, sectionsOrder };
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), { file: STATE_FILE });
    return { slugs: {}, sectionsOrder: [] };
  }
}

function writeWtState(state: WtState): void {
  try {
    mkdirSync(dirname(STATE_FILE), { recursive: true });
    writeFileSync(STATE_FILE, `${JSON.stringify(state, null, 2)}\n`);
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), { file: STATE_FILE });
    // Re-raise so the action layer can surface the failure to the
    // user (toast + event log). Silently swallowing here would
    // present a successful move while the state file is unchanged.
    throw err;
  }
}

/**
 * Drop sections from `sectionsOrder` that no slug references. Called
 * defensively after every mutation so a section that becomes empty
 * (last member archived/destroyed/moved-out) doesn't linger as a
 * ghost entry that re-anchors at the next slug placement.
 */
function prunedSectionsOrder(state: WtState): string[] {
  const live = new Set<string>();
  for (const v of Object.values(state.slugs)) {
    if (v.section !== null) live.add(v.section);
  }
  return state.sectionsOrder.filter((s) => live.has(s));
}

function ensureSection(state: WtState, section: string): WtState {
  if (state.sectionsOrder.includes(section)) return state;
  return { ...state, sectionsOrder: [...state.sectionsOrder, section] };
}

/** Drop a single slug's entry. No-op if absent. */
export function clearSlugState(slug: string): void {
  const state = readWtState();
  if (!(slug in state.slugs)) return;
  const next = { ...state, slugs: { ...state.slugs } };
  delete next.slugs[slug];
  next.sectionsOrder = prunedSectionsOrder(next);
  writeWtState(next);
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
  next.slugs[slug] = { section, order };
  next.sectionsOrder = prunedSectionsOrder(next);
  writeWtState(next);
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
  const state = readWtState();
  const next: WtState = { ...state, slugs: { ...state.slugs } };
  const min = minOrderIn(next, section);
  const baseline = min === null ? 0 : min;
  for (let i = 0; i < bucketDisplay.length; i++) {
    const slug = bucketDisplay[i]!;
    next.slugs[slug] = { section, order: baseline + i };
  }
  const a = next.slugs[slugA];
  const b = next.slugs[slugB];
  if (!a || !b) return;
  next.slugs[slugA] = { ...a, order: b.order };
  next.slugs[slugB] = { ...b, order: a.order };
  writeWtState(next);
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
  } else {
    for (const [k, v] of Object.entries(next.slugs)) {
      if (v.section === oldName) next.slugs[k] = { ...v, section: trimmed };
    }
    // Replace oldName with trimmed in-place so the section keeps its
    // display position.
    next.sectionsOrder = next.sectionsOrder.map((s) => (s === oldName ? trimmed : s));
  }
  next.sectionsOrder = prunedSectionsOrder(next);
  writeWtState(next);
}

/**
 * Reap stale slug entries against the live slug set. Called after
 * destroys to keep the state file tidy. No-op when nothing to drop.
 */
export function reapWtState(liveSlugs: ReadonlySet<string>): void {
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
}
