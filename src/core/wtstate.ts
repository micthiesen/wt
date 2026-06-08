import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

import { config } from "./config.ts";
import { createLogger } from "./logger.ts";

const STATE_FILE = join(homedir(), ".cache", "wt", "state.json");
const log = createLogger("[wtstate]");

export type WtSlugState = {
  /** Section name. `null` = unsectioned (rendered at top, no header). */
  section: string | null;
  /** Manual ordering scalar within (section, archived) bucket. Lower = earlier. */
  order: number;
};

/** Lifecycle of a single slice as it moves from plan to landed PR. */
export type StackSliceStatus = "planned" | "open" | "merged";

/**
 * One slice of a holistic change — a small, reviewable unit that becomes
 * a single draft PR. `base` is either the trunk base name (a lane root,
 * independent PR off trunk) or the `id` of another slice in the same
 * manifest (a stacked child). `dependsOn` lists slice ids that must
 * materialize first; an empty list + trunk `base` is a parallel lane.
 */
export type StackSlice = {
  id: string;
  /** 1-based stack order. Encodes the `-NN-` ordinal in the branch name. */
  ordinal: number;
  title: string;
  branch: string;
  /** Trunk base name (lane root) or another slice's `id` (stacked child). */
  base: string;
  /** Slice ids this one stacks on. Empty = lane root. */
  dependsOn: string[];
  /** File-level partition of the holistic diff owned by this slice. */
  files: string[];
  /** GitHub PR number once materialized; `null` while planned. */
  pr: number | null;
  status: StackSliceStatus;
  /** Sanctioned escape hatch: an indivisible unit over the advisory budget. */
  oversized: boolean;
  oversizedReason?: string;
};

/** Advisory size budget for a stack. Never a hard gate (see brief). */
export type StackLimits = { files: number; prodLines: number; hard: boolean };

/**
 * The authoritative description of a stack's shape. wt owns this; the
 * `stack` engine's `.git/stack/state.json` is a regenerable projection
 * of it, never a source of truth. The holistic origin is held
 * separately so wt can render it as a distinct node and slices can
 * reach the original conversation via `holisticSessionId`.
 */
export type StackManifest = {
  stackId: string;
  issue: string;
  holisticBranch: string;
  holisticSlug: string;
  /** Lets a slice find the full holistic conversation via `/history`. */
  holisticSessionId?: string;
  /** Set once `wt stack apply` tags the holistic branch. */
  archivedTag?: string;
  limits: StackLimits;
  engine: string;
  slices: StackSlice[];
};

/**
 * Persisted state for the worktree list:
 *  - `slugs`: per-worktree manual section + within-section order.
 *  - `sectionsOrder`: explicit display order for manual named sections.
 *  - `stacks`: per-feature stack manifests keyed by `stackId`. The
 *    single authoritative description of every managed stack's shape;
 *    everything else (engine links, draft PRs, and the worktree-list
 *    stack rendering — sections, order, tree) is DERIVED from it. There
 *    is no manual stack-section state: a stack exists iff a manifest does.
 *
 * Why an explicit array instead of deriving section position from
 * `min(order)` of members: derived ordering causes a section to leap
 * up or down whenever its first item moves out, which the user noticed
 * as "weird unexpected reordering". Manual sections still feel ephemeral
 * (auto-appended on first encounter, pruned when no slug references
 * them) — this array is just a sort hint, not user-managed metadata.
 */
export type WtState = {
  slugs: Record<string, WtSlugState>;
  sectionsOrder: string[];
  stacks: Record<string, StackManifest>;
};

/** Coerce one persisted slice entry, dropping anything malformed. */
function parseSlice(v: unknown): StackSlice | null {
  if (!v || typeof v !== "object") return null;
  const rec = v as Partial<StackSlice>;
  if (typeof rec.id !== "string" || rec.id.trim() === "") return null;
  if (typeof rec.branch !== "string" || rec.branch.trim() === "") return null;
  const ordinal = typeof rec.ordinal === "number" && Number.isFinite(rec.ordinal) ? rec.ordinal : 0;
  const status: StackSliceStatus =
    rec.status === "open" || rec.status === "merged" ? rec.status : "planned";
  return {
    id: rec.id,
    ordinal,
    title: typeof rec.title === "string" ? rec.title : rec.id,
    branch: rec.branch,
    base: typeof rec.base === "string" && rec.base.trim() !== "" ? rec.base : config.branch.base,
    dependsOn: Array.isArray(rec.dependsOn) ? rec.dependsOn.filter((d): d is string => typeof d === "string") : [],
    files: Array.isArray(rec.files) ? rec.files.filter((f): f is string => typeof f === "string") : [],
    pr: typeof rec.pr === "number" && Number.isFinite(rec.pr) ? rec.pr : null,
    status,
    oversized: rec.oversized === true,
    ...(typeof rec.oversizedReason === "string" ? { oversizedReason: rec.oversizedReason } : {}),
  };
}

/** Coerce one persisted manifest, dropping anything malformed. */
function parseManifest(v: unknown): StackManifest | null {
  if (!v || typeof v !== "object") return null;
  const rec = v as Partial<StackManifest>;
  if (typeof rec.stackId !== "string" || rec.stackId.trim() === "") return null;
  const limitsRaw = (rec.limits ?? {}) as Partial<StackLimits>;
  const limits: StackLimits = {
    files: typeof limitsRaw.files === "number" ? limitsRaw.files : 0,
    prodLines: typeof limitsRaw.prodLines === "number" ? limitsRaw.prodLines : 0,
    hard: limitsRaw.hard === true,
  };
  const slices = Array.isArray(rec.slices)
    ? rec.slices.map(parseSlice).filter((s): s is StackSlice => s !== null)
    : [];
  return {
    stackId: rec.stackId,
    issue: typeof rec.issue === "string" ? rec.issue : rec.stackId,
    holisticBranch: typeof rec.holisticBranch === "string" ? rec.holisticBranch : "",
    holisticSlug: typeof rec.holisticSlug === "string" ? rec.holisticSlug : "",
    ...(typeof rec.holisticSessionId === "string" ? { holisticSessionId: rec.holisticSessionId } : {}),
    ...(typeof rec.archivedTag === "string" ? { archivedTag: rec.archivedTag } : {}),
    limits,
    engine: typeof rec.engine === "string" ? rec.engine : "stack",
    slices,
  };
}

export function readWtState(): WtState {
  if (!existsSync(STATE_FILE)) return { slugs: {}, sectionsOrder: [], stacks: {} };
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
    // Self-heal: any manual section referenced by a slug but missing from
    // sectionsOrder gets appended in discovery order.
    const known = new Set(sectionsOrder);
    for (const v of Object.values(slugs)) {
      if (v.section !== null && !known.has(v.section)) {
        sectionsOrder.push(v.section);
        known.add(v.section);
      }
    }
    const stacks: Record<string, StackManifest> = {};
    if (data?.stacks && typeof data.stacks === "object") {
      for (const [k, v] of Object.entries(data.stacks)) {
        const m = parseManifest(v);
        if (m) stacks[k] = m;
      }
    }
    return { slugs, sectionsOrder, stacks };
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), { file: STATE_FILE });
    return { slugs: {}, sectionsOrder: [], stacks: {} };
  }
}

function writeWtState(state: WtState): void {
  try {
    mkdirSync(dirname(STATE_FILE), { recursive: true });
    // Write-then-rename so a concurrent reader (the live TUI polls this
    // file) never observes a half-written file and silently falls back
    // to empty defaults. rename(2) is atomic within a filesystem. This
    // closes the torn-read window; it does NOT serialise lost updates
    // between two writers (that would need a cross-process lock spanning
    // read-modify-write — out of scope here).
    const tmp = `${STATE_FILE}.${process.pid}.tmp`;
    writeFileSync(tmp, `${JSON.stringify(state, null, 2)}\n`);
    renameSync(tmp, STATE_FILE);
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), { file: STATE_FILE });
    // Re-raise so the action layer can surface the failure to the
    // user (toast + event log). Silently swallowing here would
    // present a successful move while the state file is unchanged.
    throw err;
  }
}

/** Drop manual sections from `sectionsOrder` that no slug references. */
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
    const prev = next.slugs[slug];
    next.slugs[slug] = { ...prev, section, order: baseline + i };
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
 * Move a named section one slot up (`dir = -1`) or down (`dir = 1`)
 * in `sectionsOrder`. Returns true when the swap landed, false when it
 * was a no-op (section absent, or already at the boundary). Member
 * slugs keep their `order` values; only the index moves.
 */
export function moveSection(name: string, dir: -1 | 1): boolean {
  const state = readWtState();
  const idx = state.sectionsOrder.indexOf(name);
  if (idx < 0) return false;
  const target = idx + dir;
  if (target < 0 || target >= state.sectionsOrder.length) return false;
  const next: WtState = {
    ...state,
    sectionsOrder: [...state.sectionsOrder],
  };
  const tmp = next.sectionsOrder[idx]!;
  next.sectionsOrder[idx] = next.sectionsOrder[target]!;
  next.sectionsOrder[target] = tmp;
  writeWtState(next);
  return true;
}

// ---------- Stack manifests ----------

/** Every stored stack manifest, in `stackId` insertion order. */
export function listStackManifests(): StackManifest[] {
  return Object.values(readWtState().stacks);
}

/** One manifest by id, or `null` when absent. */
export function getStackManifest(stackId: string): StackManifest | null {
  return readWtState().stacks[stackId] ?? null;
}

/** Insert or replace a manifest wholesale. Keyed by `manifest.stackId`. */
export function putStackManifest(manifest: StackManifest): void {
  const state = readWtState();
  writeWtState({
    ...state,
    stacks: { ...state.stacks, [manifest.stackId]: manifest },
  });
}

/**
 * Shallow-merge a partial onto an existing manifest. No-op (returns
 * false) when the manifest is absent. `slices` is replaced wholesale
 * when present in the patch — use `updateStackSlice` for targeted edits.
 */
export function patchStackManifest(
  stackId: string,
  patch: Partial<StackManifest>,
): boolean {
  const state = readWtState();
  const prev = state.stacks[stackId];
  if (!prev) return false;
  writeWtState({
    ...state,
    stacks: { ...state.stacks, [stackId]: { ...prev, ...patch } },
  });
  return true;
}

/**
 * Patch a single slice within a manifest (e.g. record its `pr` and flip
 * `status` to "open" after materialization). No-op (false) when the
 * manifest or slice is absent.
 */
export function updateStackSlice(
  stackId: string,
  sliceId: string,
  patch: Partial<StackSlice>,
): boolean {
  const state = readWtState();
  const prev = state.stacks[stackId];
  if (!prev) return false;
  let hit = false;
  const slices = prev.slices.map((s) => {
    if (s.id !== sliceId) return s;
    hit = true;
    return { ...s, ...patch };
  });
  if (!hit) return false;
  writeWtState({
    ...state,
    stacks: { ...state.stacks, [stackId]: { ...prev, slices } },
  });
  return true;
}

/** Drop a manifest. Returns true when one was removed. */
export function removeStackManifest(stackId: string): boolean {
  const state = readWtState();
  if (!state.stacks[stackId]) return false;
  const stacks = { ...state.stacks };
  delete stacks[stackId];
  writeWtState({ ...state, stacks });
  return true;
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
