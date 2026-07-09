import {
  coerceHunkContext,
  isUnsafeSlicePath,
  spreadHunkContext,
} from "./types.ts";
import type { PartialFile, StackLimits, StackManifest, StackSlice } from "./types.ts";

export type ManifestValidation =
  | { ok: true; manifest: StackManifest }
  | { ok: false; errors: string[] };

/** Top-level keys a manifest may carry. Anything else is a typo / drift. */
const MANIFEST_KEYS = new Set([
  "stackId", "issue", "holisticBranch", "holisticSlug", "holisticSessionId",
  "archivedTag", "limits", "engine", "hunkContext", "slices",
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
  if ("hunkContext" in rec && coerceHunkContext(rec.hunkContext) === undefined) {
    errors.push(`"hunkContext" must be a non-negative integer`);
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
    ...spreadHunkContext(rec.hunkContext),
    slices,
  };
  return { ok: true, manifest };
}
