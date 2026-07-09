import { tryAcquireLock } from "../locks.ts";
import {
  getStackManifest,
  putStackManifest,
  validateStackManifest,
  type PartialFile,
  type StackManifest,
  type StackSlice,
} from "../wtstate.ts";
import { STACK_BUSY, STACK_LOCK_SLUG } from "./shared.ts";

// ---------- split (reshape a live stack) ----------

/** One sub-slice in a `splitStack` fragment — the planner (`/split`) authors these. */
export type SubSliceSpec = {
  id: string;
  title: string;
  branch: string;
  files: string[];
  partials?: PartialFile[];
  oversized?: boolean;
  oversizedReason?: string;
};

export type SplitResult =
  | {
      ok: true;
      newSliceIds: string[];
      /** Branch the new sub-slices reproduce their files from at materialize. */
      sourceBranch: string;
      /** Branch + PR of the replaced slice — supersede (close PR, delete branch) after apply. */
      supersededBranch: string;
      supersededPr: number | null;
      /** Branches of children re-threaded onto the new tip (need a `replay`). */
      rethreadedChildren: string[];
      /** The reshaped slice list (returned for `--plan` preview; written unless planning). */
      slices: StackSlice[];
    }
  | { ok: false; error: string };

/**
 * Reshape a live stack: replace one OPEN (or still-planned) slice with the N
 * sub-slices in `fragment`, chaining them in order and re-threading the
 * replaced slice's children onto the LAST sub-slice (the new tip). Pure
 * manifest bookkeeping — no git, no PRs — mirroring `reconcileStack`. The
 * caller then runs `wt stack apply` to materialize the new sub-slice branches
 * (sourced from the replaced slice's own branch, recorded as their `source`,
 * since it carries a refactor the pre-split holistic branch predates) and
 * `wt stack replay`/`R` to rebase the descendants onto the new tip.
 *
 * The replaced slice is REMOVED from the manifest, but its branch/PR are left
 * on GitHub: the branch is the materialize source, so it must survive until
 * `apply` — close the PR + delete the branch as superseded afterwards.
 *
 * Strictly validates the reshaped manifest via `validateStackManifest`
 * (catches duplicate ids/branches, dangling deps, empty file sets) before
 * writing. `opts.plan` validates + returns the new shape WITHOUT writing.
 */
export function splitStack(
  stackId: string,
  sliceId: string,
  fragment: SubSliceSpec[],
  opts: { plan?: boolean },
): SplitResult {
  // Sync function, so no patient wait — a non-blocking probe is enough to
  // keep a reshape from interleaving with a live replay/apply. A pure
  // `--plan` preview writes nothing and can skip the lock.
  const lock = opts.plan ? null : tryAcquireLock(STACK_LOCK_SLUG, "stack", { phase: "split" });
  if (!opts.plan && !lock) return { ok: false, error: STACK_BUSY };
  try {
    return splitStackLocked(stackId, sliceId, fragment, opts);
  } finally {
    lock?.release();
  }
}

function splitStackLocked(
  stackId: string,
  sliceId: string,
  fragment: SubSliceSpec[],
  opts: { plan?: boolean },
): SplitResult {
  const manifest = getStackManifest(stackId);
  if (!manifest) return { ok: false, error: `no stack manifest: ${stackId}` };
  const target = manifest.slices.find((s) => s.id === sliceId);
  if (!target) return { ok: false, error: `no slice "${sliceId}" in ${stackId}` };
  if (target.status === "merged") {
    return { ok: false, error: `slice ${sliceId} is merged — cannot re-split a landed slice` };
  }
  if (fragment.length < 2) {
    return { ok: false, error: `split needs ≥2 sub-slices (got ${fragment.length})` };
  }
  // Reusing the replaced slice's own id/branch would make a sub-slice source
  // itself — reject it so the partition stays unambiguous.
  for (const spec of fragment) {
    if (spec.id === target.id) return { ok: false, error: `sub-slice id "${spec.id}" reuses the slice being split` };
    if (spec.branch === target.branch) return { ok: false, error: `sub-slice branch "${spec.branch}" reuses the slice being split` };
  }

  // The sub-slices reproduce their files from the original slice's branch (it
  // carries the refactor); a still-planned target has no branch yet, so fall
  // back to whatever the target itself would have materialized from.
  const source = target.status === "planned" ? target.source : target.branch;

  // Build the sub-chain: the first sub-slice takes the target's place in the
  // graph (inherits its base + dependsOn); each later one stacks on the
  // previous. All start planned, content sourced from the original branch.
  const lastSubId = fragment[fragment.length - 1]!.id;
  const subSlices: StackSlice[] = fragment.map((spec, i) => ({
    id: spec.id,
    ordinal: 0, // renumbered below
    title: spec.title,
    branch: spec.branch,
    base: i === 0 ? target.base : fragment[i - 1]!.id,
    dependsOn: i === 0 ? [...target.dependsOn] : [fragment[i - 1]!.id],
    files: spec.files,
    ...(spec.partials && spec.partials.length > 0 ? { partials: spec.partials } : {}),
    pr: null,
    status: "planned" as const,
    oversized: spec.oversized === true,
    ...(spec.oversizedReason ? { oversizedReason: spec.oversizedReason } : {}),
    ...(source ? { source } : {}),
  }));

  // Re-thread the target's children onto the new tip (the last sub-slice).
  // A child may reference the target by slice id OR by branch name — `base`
  // is stored either way (`dependsOn` is always ids) — so match both and
  // normalize the rewrite to the sub-slice id (`resolveParentBranch` resolves
  // it to the branch, and the id is stable before the branch is materialized).
  const rethreaded: string[] = [];
  const refsTarget = (ref: string): boolean => ref === target.id || ref === target.branch;
  const rethread = (s: StackSlice): StackSlice => {
    const changed = refsTarget(s.base) || s.dependsOn.some(refsTarget);
    if (changed) rethreaded.push(s.branch);
    return {
      ...s,
      base: refsTarget(s.base) ? lastSubId : s.base,
      dependsOn: s.dependsOn.map((d) => (refsTarget(d) ? lastSubId : d)),
    };
  };

  // Splice the sub-chain into the target's slot (preserving display order),
  // re-threading every other slice, then renumber ordinals over the new order.
  // The branch `-NN-` token is historical and may now diverge from `ordinal`.
  const inOrder = [...manifest.slices].sort((a, b) => a.ordinal - b.ordinal);
  const reshaped: StackSlice[] = [];
  for (const s of inOrder) {
    if (s.id === target.id) reshaped.push(...subSlices);
    else reshaped.push(rethread(s));
  }
  reshaped.forEach((s, i) => {
    s.ordinal = i + 1;
  });

  const next: StackManifest = { ...manifest, slices: reshaped };
  const v = validateStackManifest(next);
  if (!v.ok) {
    return { ok: false, error: `reshaped manifest invalid:\n  ${v.errors.join("\n  ")}` };
  }
  if (!opts.plan) putStackManifest(v.manifest);
  return {
    ok: true,
    newSliceIds: subSlices.map((s) => s.id),
    sourceBranch: source ?? manifest.holisticBranch,
    supersededBranch: target.branch,
    supersededPr: target.pr,
    rethreadedChildren: rethreaded,
    slices: v.manifest.slices,
  };
}
