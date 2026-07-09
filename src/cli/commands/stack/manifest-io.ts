import { readFileSync, statSync } from "node:fs";

import { config } from "../../../core/config.ts";
import { validateFileCoverage, type SubSliceSpec } from "../../../core/stack-ops.ts";
import {
  coercePartials,
  getStackManifest,
  putStackManifest,
  validateStackManifest,
} from "../../../core/wtstate.ts";
import { red } from "../../colors.ts";

export type IngestResult =
  | { ok: true; stackId: string; sliceCount: number }
  | { ok: false };

/**
 * Read + STRICT-validate a skill-authored manifest file, then store it
 * via `putStackManifest`. This is the ONLY boundary by which a manifest
 * enters wt state — skills never write `state.json`. Validation is two-stage:
 * structural (`validateStackManifest`) then real-diff whole-file coverage
 * (`validateFileCoverage`), both BEFORE the write, so a mis-partitioned manifest
 * (e.g. a rename whose delete-half no slice claims) never enters state. Any
 * error prints verbatim and the manifest is NOT stored.
 */
export async function ingestManifest(file: string): Promise<IngestResult> {
  let text: string;
  try {
    if (!statSync(file).isFile()) {
      console.error(red(`not a file: ${file}`));
      return { ok: false };
    }
    text = readFileSync(file, "utf8");
  } catch (e) {
    console.error(red(`cannot read ${file}: ${e instanceof Error ? e.message : String(e)}`));
    return { ok: false };
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch (e) {
    console.error(red(`invalid JSON in ${file}: ${e instanceof Error ? e.message : String(e)}`));
    return { ok: false };
  }
  const v = validateStackManifest(parsed);
  if (!v.ok) {
    console.error(
      red(`manifest validation failed (${v.errors.length} error${v.errors.length === 1 ? "" : "s"}):`),
    );
    for (const err of v.errors) console.error(red(`  • ${err}`));
    return { ok: false };
  }
  // Refuse to clobber an already-materialized stack: a wholesale replace
  // would drop the `pr`/`status` mutations apply recorded. Re-materialize
  // via `wt stack apply <stackId>` instead (it's idempotent).
  const existing = getStackManifest(v.manifest.stackId);
  if (existing && existing.slices.some((s) => s.status !== "planned")) {
    console.error(
      red(
        `stack ${v.manifest.stackId} is already materialized (has open/merged slices) — ` +
          `re-ingesting would discard recorded PRs. Run \`wt stack apply ${v.manifest.stackId}\` instead.`,
      ),
    );
    return { ok: false };
  }
  // Real-diff gate before persisting: every changed path (incl. deletions and
  // both halves of a rename) must be claimed by a slice, else it lingers from
  // base and breaks the slice that removes what depends on it. `apply` re-checks
  // at materialize time; running it here keeps an invalid manifest out of state.
  const coverageError = await validateFileCoverage(v.manifest, config.paths.mainClone);
  if (coverageError) {
    console.error(red(`whole-file coverage check failed: ${coverageError}`));
    return { ok: false };
  }
  try {
    putStackManifest(v.manifest);
  } catch (e) {
    console.error(red(`cannot store manifest: ${e instanceof Error ? e.message : String(e)}`));
    return { ok: false };
  }
  return { ok: true, stackId: v.manifest.stackId, sliceCount: v.manifest.slices.length };
}

/**
 * Read + structurally validate a `split` fragment: an array of sub-slice
 * specs (or `{ "into": [...] }`). Deep manifest validation happens in
 * `splitStack` (which runs the reshaped manifest through
 * `validateStackManifest`); this just guards the file shape.
 */
export function readFragment(file: string): SubSliceSpec[] | null {
  let text: string;
  try {
    if (!statSync(file).isFile()) {
      console.error(red(`not a file: ${file}`));
      return null;
    }
    text = readFileSync(file, "utf8");
  } catch (e) {
    console.error(red(`cannot read ${file}: ${e instanceof Error ? e.message : String(e)}`));
    return null;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch (e) {
    console.error(red(`invalid JSON in ${file}: ${e instanceof Error ? e.message : String(e)}`));
    return null;
  }
  const arr = Array.isArray(parsed)
    ? parsed
    : parsed && typeof parsed === "object" && Array.isArray((parsed as Record<string, unknown>).into)
      ? ((parsed as Record<string, unknown>).into as unknown[])
      : null;
  if (!arr) {
    console.error(red(`fragment must be an array of sub-slices or { "into": [...] }`));
    return null;
  }
  const specs: SubSliceSpec[] = [];
  const errs: string[] = [];
  arr.forEach((v, i) => {
    if (!v || typeof v !== "object") {
      errs.push(`into[${i}]: not an object`);
      return;
    }
    const r = v as Record<string, unknown>;
    if (typeof r.id !== "string" || r.id.trim() === "") errs.push(`into[${i}]: "id" required`);
    if (typeof r.branch !== "string" || r.branch.trim() === "") errs.push(`into[${i}]: "branch" required`);
    const files = Array.isArray(r.files)
      ? r.files.filter((f): f is string => typeof f === "string" && f.trim() !== "")
      : [];
    // Shared lenient coercion (same shape the schema read path uses);
    // `validateStackManifest` is the strict net on the reshaped manifest.
    const partials = coercePartials(r.partials);
    // A sub-slice must own something — whole files or hunks. The strict
    // `validateStackManifest` re-checks the reshaped manifest as the net.
    if (files.length === 0 && partials.length === 0) {
      errs.push(`into[${i}]: needs a non-empty "files" or "partials"`);
    }
    if (typeof r.id !== "string" || typeof r.branch !== "string") return;
    specs.push({
      id: r.id,
      title: typeof r.title === "string" ? r.title : r.id,
      branch: r.branch,
      files,
      ...(partials.length > 0 ? { partials } : {}),
      oversized: r.oversized === true,
      ...(typeof r.oversizedReason === "string" ? { oversizedReason: r.oversizedReason } : {}),
    });
  });
  if (errs.length > 0) {
    console.error(red(`fragment validation failed:`));
    for (const e of errs) console.error(red(`  • ${e}`));
    return null;
  }
  return specs;
}
