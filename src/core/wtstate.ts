import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

import { config } from "./config.ts";
import { withFileLock } from "./locks.ts";
import { createLogger } from "./logger.ts";

const STATE_FILE = join(homedir(), ".cache", "wt", "state.json");
const log = createLogger("[wtstate]");

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

export type ManifestValidation =
  | { ok: true; manifest: StackManifest }
  | { ok: false; errors: string[] };

/** Top-level keys a manifest may carry. Anything else is a typo / drift. */
const MANIFEST_KEYS = new Set([
  "stackId", "issue", "holisticBranch", "holisticSlug", "holisticSessionId",
  "archivedTag", "limits", "engine", "slices",
]);
/** Per-slice keys a slice may carry. */
const SLICE_KEYS = new Set([
  "id", "ordinal", "title", "branch", "base", "dependsOn", "files", "pr",
  "status", "oversized", "oversizedReason", "baseSha", "source", "partials",
]);

/**
 * STRICT validation for INGESTING a skill-authored manifest (the
 * `wt stack apply --from <file>` path). Unlike the lenient `parseManifest`
 * read path — which silently coerces/drops malformed fields so wt's own
 * state always loads — this fails LOUD: it never coerces, and returns the
 * full list of problems so a typo can't materialize a subtly-wrong stack.
 *
 * Errors on: unknown keys (top-level or per-slice, so schema drift is
 * visible); missing/empty `stackId`/`issue`/`holisticBranch`/`holisticSlug`
 * or empty `slices`; a slice missing `id`/`branch`/`ordinal`/`base` or with
 * an empty `files`; `base` equal to the slice's own `id`; `dependsOn` ids
 * that don't match another slice or that point at the slice itself;
 * duplicate slice `id`s or `branch`es; `oversized: true` without a non-empty
 * `oversizedReason`; a non-`planned` `status` with no `pr` (which `apply`
 * would silently skip); and any present field of the wrong type. `base`
 * accepts ANY non-empty branch string (a stack can be stacked on an unmerged
 * parent PR). Dependency cycles are caught later by `topoSortSlices` at
 * materialize time.
 */
export function validateStackManifest(raw: unknown): ManifestValidation {
  const errors: string[] = [];
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) {
    return { ok: false, errors: ["manifest must be a JSON object"] };
  }
  const rec = raw as Record<string, unknown>;
  for (const k of Object.keys(rec)) {
    if (!MANIFEST_KEYS.has(k)) errors.push(`unknown top-level key: "${k}"`);
  }

  const reqStr = (key: string): string | null => {
    const v = rec[key];
    if (typeof v !== "string" || v.trim() === "") {
      errors.push(`"${key}" is required (non-empty string)`);
      return null;
    }
    return v;
  };
  const stackId = reqStr("stackId");
  const issue = reqStr("issue");
  const holisticBranch = reqStr("holisticBranch");
  const holisticSlug = reqStr("holisticSlug");
  for (const key of ["holisticSessionId", "archivedTag", "engine"] as const) {
    if (key in rec && typeof rec[key] !== "string") errors.push(`"${key}" must be a string`);
  }

  let limits: StackLimits = { files: 0, prodLines: 0, hard: false };
  if ("limits" in rec) {
    const l = rec.limits;
    if (!l || typeof l !== "object" || Array.isArray(l)) {
      errors.push(`"limits" must be an object`);
    } else {
      const lr = l as Record<string, unknown>;
      const files = lr.files;
      const prodLines = lr.prodLines;
      const hard = lr.hard;
      if (typeof files !== "number" || !Number.isFinite(files)) errors.push(`"limits.files" must be a number`);
      if (typeof prodLines !== "number" || !Number.isFinite(prodLines)) errors.push(`"limits.prodLines" must be a number`);
      if (hard !== undefined && typeof hard !== "boolean") errors.push(`"limits.hard" must be a boolean`);
      if (typeof files === "number" && typeof prodLines === "number") {
        limits = { files, prodLines, hard: hard === true };
      }
    }
  }

  const slicesRaw = rec.slices;
  if (!Array.isArray(slicesRaw) || slicesRaw.length === 0) {
    errors.push(`"slices" is required (non-empty array)`);
  }
  const slices: StackSlice[] = [];
  const idCounts = new Map<string, number>();
  const branchCounts = new Map<string, number>();
  if (Array.isArray(slicesRaw)) {
    slicesRaw.forEach((sv, i) => {
      const at = `slices[${i}]`;
      if (!sv || typeof sv !== "object" || Array.isArray(sv)) {
        errors.push(`${at} must be an object`);
        return;
      }
      const s = sv as Record<string, unknown>;
      for (const k of Object.keys(s)) {
        if (!SLICE_KEYS.has(k)) errors.push(`${at}: unknown key "${k}"`);
      }
      const id = typeof s.id === "string" && s.id.trim() !== "" ? s.id : null;
      if (!id) errors.push(`${at}: "id" is required (non-empty string)`);
      else idCounts.set(id, (idCounts.get(id) ?? 0) + 1);
      const branch = typeof s.branch === "string" && s.branch.trim() !== "" ? s.branch : null;
      if (!branch) errors.push(`${at}: "branch" is required (non-empty string)`);
      else branchCounts.set(branch, (branchCounts.get(branch) ?? 0) + 1);
      // `partials` (hunk-level ownership) parsed first: a slice may own
      // nothing whole-file as long as it owns hunks, so the `files`
      // requirement relaxes to "files OR partials non-empty".
      const partialsClean: PartialFile[] = [];
      let partialsOk = true;
      if (s.partials !== undefined) {
        if (!Array.isArray(s.partials)) {
          errors.push(`${at}: "partials" must be an array`);
          partialsOk = false;
        } else {
          s.partials.forEach((pv, pi) => {
            const pat = `${at}.partials[${pi}]`;
            if (!pv || typeof pv !== "object" || Array.isArray(pv)) {
              errors.push(`${pat} must be an object`);
              partialsOk = false;
              return;
            }
            const p = pv as Record<string, unknown>;
            for (const k of Object.keys(p)) {
              if (k !== "file" && k !== "hunks") errors.push(`${pat}: unknown key "${k}"`);
            }
            const file = typeof p.file === "string" && p.file.trim() !== "" ? p.file : null;
            if (!file) {
              errors.push(`${pat}: "file" is required (non-empty string)`);
              partialsOk = false;
            }
            const hunksOk =
              Array.isArray(p.hunks) &&
              p.hunks.length > 0 &&
              p.hunks.every((h) => typeof h === "string" && h.trim() !== "");
            if (!hunksOk) {
              errors.push(`${pat}: "hunks" is required (non-empty array of hunk ids)`);
              partialsOk = false;
            }
            if (file && hunksOk) {
              partialsClean.push({ file, hunks: (p.hunks as string[]).slice() });
            }
          });
        }
      }
      const filesArrOk =
        s.files === undefined ||
        (Array.isArray(s.files) && s.files.every((f) => typeof f === "string" && f.trim() !== ""));
      if (!filesArrOk) errors.push(`${at}: "files" must be an array of non-empty paths`);
      const filesList = Array.isArray(s.files)
        ? (s.files as unknown[]).filter((f): f is string => typeof f === "string" && f.trim() !== "")
        : [];
      // A slice must own SOMETHING — whole files or hunks.
      const ownsSomething = filesList.length > 0 || partialsClean.length > 0;
      if (!ownsSomething) {
        errors.push(`${at}: a slice must own at least one whole file ("files") or hunk set ("partials")`);
      }
      // Within a slice, a file can't be both whole-owned and hunk-owned, and
      // can't appear twice in partials.
      const partialFilesHere = new Set<string>();
      for (const p of partialsClean) {
        if (partialFilesHere.has(p.file)) errors.push(`${at}: file "${p.file}" appears twice in "partials"`);
        partialFilesHere.add(p.file);
        if (filesList.includes(p.file)) {
          errors.push(`${at}: file "${p.file}" is in both "files" (whole) and "partials" (hunks)`);
        }
      }
      // Reject paths that would escape the worktree at materialize.
      for (const f of [...filesList, ...partialsClean.map((p) => p.file)]) {
        if (isUnsafeSlicePath(f)) errors.push(`${at}: file "${f}" must be a repo-relative path (no "..", not absolute)`);
      }
      const sliceShapeOk = filesArrOk && partialsOk && ownsSomething;
      const ordinalOk = typeof s.ordinal === "number" && Number.isFinite(s.ordinal);
      if (!ordinalOk) errors.push(`${at}: "ordinal" is required (finite number)`);
      const base = typeof s.base === "string" && s.base.trim() !== "" ? s.base : null;
      if (!base) errors.push(`${at}: "base" is required (non-empty branch string)`);
      else if (id && base === id) errors.push(`${at}: "base" cannot be the slice's own id "${base}"`);
      const depsOk =
        s.dependsOn === undefined ||
        (Array.isArray(s.dependsOn) && s.dependsOn.every((d) => typeof d === "string"));
      if (!depsOk) errors.push(`${at}: "dependsOn" must be an array of slice ids`);
      if (s.status !== undefined && s.status !== "planned" && s.status !== "open" && s.status !== "merged") {
        errors.push(`${at}: "status" must be one of planned|open|merged`);
      }
      const hasPr = typeof s.pr === "number" && Number.isFinite(s.pr);
      if (s.pr !== undefined && s.pr !== null && !hasPr) {
        errors.push(`${at}: "pr" must be a number or null`);
      }
      // A non-planned status without a PR would be silently SKIPPED by
      // `applyStack` — exactly the "subtly-wrong stack" the strict path
      // exists to prevent. A freshly-split slice must be `planned`.
      if ((s.status === "open" || s.status === "merged") && !hasPr) {
        errors.push(`${at}: "status: ${s.status}" requires a numeric "pr" (a planned slice has none yet)`);
      }
      if ("title" in s && typeof s.title !== "string") errors.push(`${at}: "title" must be a string`);
      if (s.oversized !== undefined && typeof s.oversized !== "boolean") errors.push(`${at}: "oversized" must be a boolean`);
      if (s.oversizedReason !== undefined && typeof s.oversizedReason !== "string") errors.push(`${at}: "oversizedReason" must be a string`);
      if (s.oversized === true && (typeof s.oversizedReason !== "string" || s.oversizedReason.trim() === "")) {
        errors.push(`${at}: "oversized: true" requires a non-empty "oversizedReason"`);
      }
      if (s.baseSha !== undefined && typeof s.baseSha !== "string") errors.push(`${at}: "baseSha" must be a string`);
      if (s.source !== undefined && typeof s.source !== "string") errors.push(`${at}: "source" must be a string`);
      if (id && branch && sliceShapeOk && ordinalOk && base) {
        slices.push({
          id,
          ordinal: s.ordinal as number,
          title: typeof s.title === "string" ? s.title : id,
          branch,
          base,
          dependsOn: Array.isArray(s.dependsOn)
            ? s.dependsOn.filter((d): d is string => typeof d === "string")
            : [],
          files: filesList,
          pr: typeof s.pr === "number" ? s.pr : null,
          status: s.status === "open" || s.status === "merged" ? s.status : "planned",
          oversized: s.oversized === true,
          ...(typeof s.oversizedReason === "string" ? { oversizedReason: s.oversizedReason } : {}),
          ...(typeof s.baseSha === "string" && s.baseSha.trim() !== "" ? { baseSha: s.baseSha } : {}),
          ...(typeof s.source === "string" && s.source.trim() !== "" ? { source: s.source } : {}),
          ...(partialsClean.length > 0 ? { partials: partialsClean } : {}),
        });
      }
    });
  }
  for (const [id, n] of idCounts) if (n > 1) errors.push(`duplicate slice id: "${id}" (${n}×)`);
  for (const [b, n] of branchCounts) if (n > 1) errors.push(`duplicate slice branch: "${b}" (${n}×)`);

  // Cross-slice partition integrity. A file owned whole by one slice can't be
  // hunk-split by another, and no (file, hunk) may be claimed twice. Coverage
  // (every holistic hunk owned exactly once) needs the real diff, so it's
  // checked at `apply`, not here.
  const wholeFileOwner = new Map<string, string>();
  for (const sl of slices) for (const f of sl.files) wholeFileOwner.set(f, sl.id);
  const hunkOwner = new Map<string, string>(); // `${file}\0${hunkId}` -> sliceId
  for (const sl of slices) {
    for (const p of sl.partials ?? []) {
      const whole = wholeFileOwner.get(p.file);
      if (whole && whole !== sl.id) {
        errors.push(`file "${p.file}" is owned whole by ${whole} and hunk-split by ${sl.id} — pick one`);
      }
      for (const hid of p.hunks) {
        const key = `${p.file}\0${hid}`;
        const prev = hunkOwner.get(key);
        if (prev) errors.push(`hunk "${hid}" of "${p.file}" is owned by both ${prev} and ${sl.id}`);
        else hunkOwner.set(key, sl.id);
      }
    }
  }
  // `dependsOn` ids must reference another slice, and never self.
  const idSet = new Set(idCounts.keys());
  if (Array.isArray(slicesRaw)) {
    slicesRaw.forEach((sv, i) => {
      if (!sv || typeof sv !== "object" || Array.isArray(sv)) return;
      const s = sv as Record<string, unknown>;
      if (!Array.isArray(s.dependsOn)) return;
      for (const d of s.dependsOn) {
        if (typeof d !== "string") continue;
        if (d === s.id) errors.push(`slices[${i}]: "dependsOn" cannot include its own id "${d}"`);
        else if (!idSet.has(d)) errors.push(`slices[${i}]: "dependsOn" references unknown slice id "${d}"`);
      }
    });
  }

  if (errors.length > 0) return { ok: false, errors };
  // Narrow locally so the compiler — not the empty-`errors` discipline —
  // proves the required strings are non-null before building the manifest.
  if (stackId === null || issue === null || holisticBranch === null || holisticSlug === null) {
    return { ok: false, errors: ["internal: required field null after validation"] };
  }
  const manifest: StackManifest = {
    stackId,
    issue,
    holisticBranch,
    holisticSlug,
    ...(typeof rec.holisticSessionId === "string" ? { holisticSessionId: rec.holisticSessionId } : {}),
    ...(typeof rec.archivedTag === "string" ? { archivedTag: rec.archivedTag } : {}),
    limits,
    engine: typeof rec.engine === "string" ? rec.engine : "stack",
    slices,
  };
  return { ok: true, manifest };
}

export function readWtState(): WtState {
  if (!existsSync(STATE_FILE)) return { slugs: {}, sectionsOrder: [], stacks: {}, foldedSections: [] };
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
        if (typeof rec.baseBranch === "string" && rec.baseBranch.trim() !== "") {
          slugs[k]!.baseBranch = rec.baseBranch;
          if (typeof rec.baseSha === "string" && rec.baseSha.trim() !== "") {
            slugs[k]!.baseSha = rec.baseSha;
          }
        }
      }
    }
    // Stacks parse before the order array — the self-heal below needs
    // the live manifest set to seed/prune stack section keys.
    const stacks: Record<string, StackManifest> = {};
    if (data?.stacks && typeof data.stacks === "object") {
      for (const [k, v] of Object.entries(data.stacks)) {
        const m = parseManifest(v);
        if (m) stacks[k] = m;
      }
    }
    const rawOrder: string[] = [];
    if (Array.isArray(data?.sectionsOrder)) {
      const seen = new Set<string>();
      for (const s of data.sectionsOrder) {
        if (typeof s !== "string" || s.trim() === "") continue;
        if (seen.has(s)) continue;
        seen.add(s);
        rawOrder.push(s);
      }
    }
    const liveStackKeys = Object.keys(stacks)
      .map(stackSectionKey)
      .sort((a, b) => a.localeCompare(b));
    let sectionsOrder: string[];
    if (!rawOrder.includes(GROUP_INBOX)) {
      // Pre-unification file (manual names only): seed the unified order
      // with the legacy bucket layout so the migration changes nothing
      // visually — stacks (alphabetical, as the buckets sorted them),
      // then the inbox, then the manual sections in their stored order.
      sectionsOrder = [
        ...liveStackKeys,
        GROUP_INBOX,
        ...rawOrder.filter((s) => !s.startsWith(STACK_SECTION_PREFIX)),
      ];
    } else {
      // Drop stack keys whose manifest is gone; float new stacks to the
      // very top (matching where stack sections always appeared before
      // they were orderable). Deterministic in-memory heal — persisted
      // whenever the next mutator writes the state back.
      const kept = rawOrder.filter((s) => {
        const sid = stackIdFromSectionKey(s);
        return sid === null || sid in stacks;
      });
      const missing = liveStackKeys.filter((k) => !kept.includes(k));
      sectionsOrder = [...missing, ...kept];
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
    const foldedSections: string[] = [];
    if (Array.isArray(data?.foldedSections)) {
      const seen = new Set<string>();
      for (const s of data.foldedSections) {
        if (typeof s !== "string" || s.trim() === "" || seen.has(s)) continue;
        seen.add(s);
        foldedSections.push(s);
      }
    }
    return { slugs, sectionsOrder, stacks, foldedSections };
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), { file: STATE_FILE });
    return { slugs: {}, sectionsOrder: [], stacks: {}, foldedSections: [] };
  }
}

function writeWtState(state: WtState): void {
  try {
    mkdirSync(dirname(STATE_FILE), { recursive: true });
    // Write-then-rename so a concurrent reader (the live TUI polls this
    // file) never observes a half-written file and silently falls back
    // to empty defaults. rename(2) is atomic within a filesystem. This
    // closes the torn-read window; lost updates between two WRITERS are
    // closed separately by `withWtStateLock` spanning each mutator's
    // read-modify-write.
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

/**
 * Serialize a state-file read-modify-write across processes. The atomic
 * rename in `writeWtState` stops torn reads, but two concurrent WRITERS
 * (the TUI's startup reap vs a CLI `wt stack` mutation) each write back
 * from their own pre-write snapshot, silently dropping whichever update
 * landed in between. Every mutator below wraps its read→mutate→write in
 * this blocking flock; the critical sections are pure sync JSON work, so
 * the kernel wait is sub-millisecond and crash-safe (fd close releases).
 */
function withWtStateLock<T>(fn: () => T): T {
  return withFileLock("__wtstate__", fn);
}

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

// ---------- Stack manifests ----------

/** Every stored stack manifest, in `stackId` insertion order. */
export function listStackManifests(): StackManifest[] {
  return Object.values(readWtState().stacks);
}

/** One manifest by id, or `null` when absent. */
export function getStackManifest(stackId: string): StackManifest | null {
  return readWtState().stacks[stackId] ?? null;
}

/**
 * The stackId of the manifest that owns `branch` — matching a slice
 * branch OR the holistic origin branch — or `null` when none does. Lets
 * stack subcommands resolve their target from the current worktree's
 * branch instead of making the caller pass an id they'd have to look up
 * (the id is lower-kebab `eng-1234`, not the branch's `michael/eng-1234-…`).
 * Returns the first match; branches are unique across manifests in
 * practice, so ambiguity isn't a real case.
 */
export function findStackIdByBranch(branch: string): string | null {
  if (!branch) return null;
  for (const m of Object.values(readWtState().stacks)) {
    if (m.holisticBranch === branch) return m.stackId;
    if (m.slices.some((s) => s.branch === branch)) return m.stackId;
  }
  return null;
}

/** Insert or replace a manifest wholesale. Keyed by `manifest.stackId`. */
export function putStackManifest(manifest: StackManifest): void {
  withWtStateLock(() => {
    const state = readWtState();
    writeWtState({
      ...state,
      stacks: { ...state.stacks, [manifest.stackId]: manifest },
    });
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
  return withWtStateLock(() => {
    const state = readWtState();
    const prev = state.stacks[stackId];
    if (!prev) return false;
    writeWtState({
      ...state,
      stacks: { ...state.stacks, [stackId]: { ...prev, ...patch } },
    });
    return true;
  });
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
  return withWtStateLock(() => {
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
