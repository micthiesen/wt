import { config } from "../config.ts";

/**
 * Synthetic section key for a manifest-driven stack. NUL-prefixed so
 * it can never collide with a user's manual section name. The TUI
 * re-exports these from `useWorktreeRows.ts`; they live here because
 * `sectionsOrder` — the unified display order over ALL groups (stack
 * sections, the inbox, manual sections) — is owned by this module.
 * The value is persisted (foldedSections, sectionsOrder), so it must
 * never change.
 */
export const STACK_SECTION_PREFIX = "\0stack:";
export function stackSectionKey(stackId: string): string {
  return `${STACK_SECTION_PREFIX}${stackId}`;
}
/** Inverse of `stackSectionKey`; `null` for non-stack keys. */
export function stackIdFromSectionKey(key: string): string | null {
  return key.startsWith(STACK_SECTION_PREFIX)
    ? key.slice(STACK_SECTION_PREFIX.length)
    : null;
}

/**
 * Sentinel entry representing the unsectioned inbox in `sectionsOrder`.
 * NUL-prefixed like stack keys so it can't collide with a manual
 * name. Its presence doubles as the migration marker: a state file
 * without it predates unified group ordering and gets seeded with the
 * legacy layout (stacks, then inbox, then manual sections) on read.
 */
export const GROUP_INBOX = "\0inbox";

export type WtSlugState = {
  /** Section name. `null` = unsectioned (rendered at top, no header). */
  section: string | null;
  /** Manual ordering scalar within (section, archived) bucket. Lower = earlier. */
  order: number;
  /**
   * Branch this worktree was forked from (`wt new --base <ref>`), when
   * that ref isn't trunk. Display/diff hint only — the stack manifest
   * stays the sole input to the restack engine; a manifest slice's
   * parent always wins over this. Cleared when the branch is promoted
   * into a stack (`wt stack add`) or via `wt base clear`.
   */
  baseBranch?: string;
  /**
   * Fork-point sha recorded alongside `baseBranch` at creation. Free to
   * capture then, and gives a later `wt stack add` a squash-safe anchor
   * even if the parent advances or lands first.
   */
  baseSha?: string;
  /**
   * Per-worktree opt-out from `[[automations]]` (Ctrl+A in the TUI).
   * Present only when true; the engine skips paused slugs entirely
   * (no fires, no queued intents).
   */
  automationsPaused?: boolean;
};

/**
 * History entry for a destroyed worktree — powers the TUI's removed-
 * worktrees view (`h`) and its restore action. Snapshotted at destroy
 * dispatch by the TUI flows (rich: title + PR) and confirmed by
 * `removeWorktree` itself on success (minimal: slug + branch), so CLI
 * removes are tracked too. Merged by slug: defined fields of a newer
 * record win, rich fields survive a later minimal write.
 */
export type RemovedWorktree = {
  slug: string;
  branch: string;
  /** ISO timestamp of the latest destroy dispatch / completion. */
  removedAt: string;
  /** Display title at removal (AI/PR/commit-derived; absent when it was just the slug). */
  title?: string;
  /** PR snapshot at removal, when the branch had one. */
  prNumber?: number;
  prUrl?: string;
  prState?: string;
};

/** Lifecycle of a single slice as it moves from plan to landed PR. */
export type StackSliceStatus = "planned" | "open" | "merged";

/**
 * A file this slice owns only PART of: the listed `hunks` (stable
 * content-hash ids from the holistic diff, see `core/hunks.ts`) rather than
 * the whole file. Lets a single changed file span multiple slices. A file is
 * in a slice's `files` (whole) OR some slice's `partials` (by hunk), never
 * both; across the stack the hunks of a partial file must cover its holistic
 * diff exactly (checked at `apply` against the real diff).
 */
export type PartialFile = {
  file: string;
  hunks: string[];
};

/**
 * One slice of a holistic change — a small, reviewable unit that becomes
 * a single draft PR. `base` is one of: the trunk base name (a lane root,
 * independent PR off trunk); the `id` of another slice in the same
 * manifest (a stacked child); or an external branch (when the whole stack
 * is itself stacked on an unmerged parent PR). `dependsOn` lists slice ids
 * that must materialize first; an empty list + trunk `base` is a parallel
 * lane.
 */
export type StackSlice = {
  id: string;
  /** 1-based stack order. Encodes the `-NN-` ordinal in the branch name. */
  ordinal: number;
  title: string;
  branch: string;
  /** Trunk base name, another slice's `id`, or an external parent branch. */
  base: string;
  /** Slice ids this one stacks on. Empty = lane root. */
  dependsOn: string[];
  /** File-level partition of the holistic diff owned by this slice. */
  files: string[];
  /**
   * Hunk-level partition: files this slice owns only part of. Optional and
   * usually absent — most slices are pure whole-file. When present, the
   * listed files are reproduced by reconstructing the owned hunks at
   * materialize instead of a whole-file checkout. See `PartialFile`.
   */
  partials?: PartialFile[];
  /** GitHub PR number once materialized; `null` while planned. */
  pr: number | null;
  status: StackSliceStatus;
  /** Sanctioned escape hatch: an indivisible unit over the advisory budget. */
  oversized: boolean;
  oversizedReason?: string;
  /**
   * Squash-safe replay anchor: the parent-tip SHA this slice's own commits
   * sit on top of. Recorded at materialize (`applyStack`) and advanced after
   * each successful replay. The native engine rebases `--onto <newParentTip>
   * <baseSha> <branch>`, so only the slice's own commits move — a parent
   * that squash-merged (its commit no longer matching) is excluded by
   * construction, no patch-id guessing. Absent on manifests authored before
   * a slice was first materialized; replay falls back to a merge-base then.
   */
  baseSha?: string;
  /**
   * Branch to reproduce this slice's files from at materialize, instead of
   * `manifest.holisticBranch`. Set when a slice is created by re-splitting
   * another slice mid-stack (`wt stack split`): the sub-slices partition the
   * ORIGINAL slice's branch, which can carry content the pre-split holistic
   * branch doesn't (e.g. a refactor committed after the stack was applied).
   */
  source?: string;
};

/** Advisory size budget for a stack. Never a hard gate (see brief). */
export type StackLimits = { files: number; prodLines: number; hard: boolean };

/**
 * The authoritative description of a stack's shape. wt owns this — the
 * single source of truth the native restack engine replays from. The
 * holistic origin is held separately so wt can render it as a distinct
 * node and slices can reach the original conversation via
 * `holisticSessionId`.
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
  /**
   * Vestigial. Named the external `stack` CLI before the engine was
   * absorbed into wt (2026-06-08); nothing reads it anymore. Kept so
   * stored manifests and skill-authored ingest JSON stay valid.
   */
  engine: string;
  /**
   * Unified-diff context-line count for hunk-level partitioning. Pins the
   * `git diff -U<n>` setting so the content-hashed hunk ids `/split` assigns
   * line up with what materialize reconstructs. Omitted ⇒ git's default of 3.
   * Set to 0 to split edits that 3 lines of shared context would otherwise
   * coalesce into a single inseparable hunk. Stack-wide (a file's ids are
   * computed at one level); harmless on whole-file-only stacks.
   */
  hunkContext?: number;
  slices: StackSlice[];
};

/**
 * Persisted state for the worktree list:
 *  - `slugs`: per-worktree manual section + within-section order.
 *  - `sectionsOrder`: the unified display order over every GROUP in the
 *    list — stack section keys (`stackSectionKey(stackId)`), the inbox
 *    sentinel (`GROUP_INBOX`), and manual section names, all in one
 *    ranked array. `readWtState` self-heals it: new stacks prepend
 *    (top of the list), dead stack keys drop, slug-referenced manual
 *    sections missing from the array append, and a pre-unification
 *    file (no inbox sentinel) is seeded with the legacy layout
 *    (stacks alphabetical, then inbox, then manual sections) so the
 *    migration is visually a no-op.
 *  - `stacks`: per-feature stack manifests keyed by `stackId`. The
 *    single authoritative description of every managed stack's shape;
 *    everything else (engine links, draft PRs, and the worktree-list
 *    stack rendering — membership, within-stack order, tree) is DERIVED
 *    from it. The stack's position among the other groups is the one
 *    display-only bit that isn't: it lives in `sectionsOrder`.
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
  /** Section keys the user has folded in the list (persisted across restarts). */
  foldedSections: string[];
  /**
   * Stack ids whose automations are paused (Ctrl+A on any stack member
   * or its folded header). Keyed by stackId rather than member slugs so
   * the pause survives stack mutation — slices added, re-split, or
   * cleaned later are covered/released together. Ids whose manifest is
   * gone are pruned on the next pause write.
   */
  pausedStacks: string[];
  /** Global automations pause (Shift+A). Persisted across restarts. */
  automationsPaused: boolean;
  /**
   * Recently destroyed worktrees, newest first. Capped + age-pruned at
   * write time (`recordRemovedWorktrees`); an entry whose slug is live
   * again is display-filtered by the TUI and cleared by `createWorktree`.
   */
  removed: RemovedWorktree[];
};

/** Coerce one persisted slice entry, dropping anything malformed. */
export function parseSlice(v: unknown): StackSlice | null {
  if (!v || typeof v !== "object") return null;
  const rec = v as Partial<StackSlice>;
  if (typeof rec.id !== "string" || rec.id.trim() === "") return null;
  if (typeof rec.branch !== "string" || rec.branch.trim() === "") return null;
  const ordinal = typeof rec.ordinal === "number" && Number.isFinite(rec.ordinal) ? rec.ordinal : 0;
  const status: StackSliceStatus =
    rec.status === "open" || rec.status === "merged" ? rec.status : "planned";
  const partials = coercePartials(rec.partials);
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
    ...(typeof rec.baseSha === "string" && rec.baseSha.trim() !== "" ? { baseSha: rec.baseSha } : {}),
    ...(typeof rec.source === "string" && rec.source.trim() !== "" ? { source: rec.source } : {}),
    ...(partials.length > 0 ? { partials } : {}),
  };
}

/**
 * A slice file path that would escape the worktree (absolute, or with a `..`
 * segment). Manifests are skill-authored, but materialize writes
 * reconstructed partial content straight to `join(wtPath, file)`, so a
 * traversal path is rejected at the schema boundary. Repo paths are always
 * relative with forward slashes.
 */
export function isUnsafeSlicePath(p: string): boolean {
  if (p.startsWith("/") || /^[a-zA-Z]:[\\/]/.test(p)) return true;
  return p.split(/[\\/]/).includes("..");
}

/** A valid hunk context (non-negative integer), or `undefined` to mean "use the default". */
export function coerceHunkContext(v: unknown): number | undefined {
  return typeof v === "number" && Number.isInteger(v) && v >= 0 ? v : undefined;
}

/** Coerce a persisted/skill-authored `partials` array, dropping malformed entries. */
export function coercePartials(v: unknown): PartialFile[] {
  if (!Array.isArray(v)) return [];
  const out: PartialFile[] = [];
  for (const e of v) {
    if (!e || typeof e !== "object") continue;
    const rec = e as Record<string, unknown>;
    if (typeof rec.file !== "string" || rec.file.trim() === "") continue;
    const hunks = Array.isArray(rec.hunks)
      ? rec.hunks.filter((h): h is string => typeof h === "string" && h.trim() !== "")
      : [];
    if (hunks.length === 0) continue;
    out.push({ file: rec.file, hunks });
  }
  return out;
}

/** Coerce one persisted manifest, dropping anything malformed. */
export function parseManifest(v: unknown): StackManifest | null {
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
    ...spreadHunkContext(rec.hunkContext),
    slices,
  };
}

/** The conditional `{ hunkContext }` spread (omitted when absent/invalid). */
export function spreadHunkContext(v: unknown): { hunkContext?: number } {
  const hc = coerceHunkContext(v);
  return hc !== undefined ? { hunkContext: hc } : {};
}
