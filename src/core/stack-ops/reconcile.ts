import { branchExists } from "../git.ts";
import { viewPrInfo } from "../github.ts";
import { isTrunkBase } from "../stack-layout.ts";
import { getStackManifest, updateStackSlice, type StackSlice } from "../wtstate.ts";
import { acquireStackLock, STACK_BUSY, type Logger } from "./shared.ts";

/**
 * Reconcile the manifest against landed reality: flip merged slices to
 * `merged`, reparent each orphaned child onto its deepest surviving
 * dependency (or trunk), and reparent a slice whose EXTERNAL parent
 * (stack-on-stack) has landed onto trunk. Manifest bookkeeping only — reads
 * GitHub/git state but never rewrites branches — so the skill can run it on
 * its own before deciding to replay.
 */
export async function reconcileStack(
  stackId: string,
  trunk: string,
  onLog: Logger,
): Promise<void> {
  const lock = await acquireStackLock("reconcile");
  if (!lock) {
    onLog(`skipped reconcile of ${stackId} — ${STACK_BUSY}`);
    return;
  }
  try {
    await reconcileStackLocked(stackId, trunk, onLog);
  } finally {
    lock.release();
  }
}

async function reconcileStackLocked(
  stackId: string,
  trunk: string,
  onLog: Logger,
): Promise<void> {
  const manifest = getStackManifest(stackId);
  if (!manifest) return;
  // Probe live PR state for every candidate slice in parallel.
  const candidates = manifest.slices.filter(
    (s) => s.pr && s.status !== "merged",
  );
  const probed = await Promise.all(
    candidates.map(async (s) => ({ s, live: await viewPrInfo(s.branch) })),
  );
  const mergedIds = new Set<string>(
    manifest.slices.filter((s) => s.status === "merged").map((s) => s.id),
  );
  for (const { s, live } of probed) {
    if (live?.state === "MERGED") {
      mergedIds.add(s.id);
      updateStackSlice(stackId, s.id, { status: "merged" });
      onLog(`slice ${s.id} merged (#${s.pr})`);
    }
  }
  if (mergedIds.size > 0) {
    // Reparent each surviving slice that lost a dependency onto its
    // deepest STILL-OPEN dependency (highest ordinal), falling to trunk
    // only when none survive. Reparenting straight to trunk would flatten
    // a slice that still has a live ancestor (diamond / multi-parent).
    const fresh = getStackManifest(stackId);
    if (!fresh) return;
    const byId = new Map(fresh.slices.map((s) => [s.id, s]));
    for (const slice of fresh.slices) {
      if (slice.status === "merged") continue;
      const dependsOn = slice.dependsOn.filter((d) => !mergedIds.has(d));
      const baseMerged = mergedIds.has(slice.base);
      if (dependsOn.length === slice.dependsOn.length && !baseMerged) continue;
      const survivingParent = dependsOn
        .map((d) => byId.get(d))
        .filter((s): s is StackSlice => !!s)
        .sort((a, b) => b.ordinal - a.ordinal)[0];
      const base = survivingParent ? survivingParent.id : trunk;
      // The list reads the parent straight from the manifest, so updating
      // `base`/`dependsOn` is all that's needed — no separate display state.
      updateStackSlice(stackId, slice.id, { dependsOn, base });
      onLog(`reparented ${slice.id} onto ${base}`);
    }
  }

  // Cross-stack reconcile: a slice stacked on an EXTERNAL parent (another
  // stack's tip, or a standalone parent PR branch) keeps a dead `base` once
  // that parent lands — the own-slice probe above only sees THIS manifest's
  // PRs, so it can't notice. Detect the external parent merged (or its
  // branch gone) and reparent onto trunk. The slice's `baseSha` anchor keeps
  // the subsequent replay squash-safe: the landed parent's commits sit below
  // the anchor and are excluded by construction, exactly like a sibling
  // squash-merge. Runs unconditionally — the external parent merging is
  // invisible to `mergedIds`.
  const after = getStackManifest(stackId);
  if (!after) return;
  const siblingIds = new Set(after.slices.map((s) => s.id));
  const siblingBranches = new Set(after.slices.map((s) => s.branch));
  for (const slice of after.slices) {
    if (slice.status === "merged") continue;
    if (slice.base === trunk || isTrunkBase(slice)) continue;
    if (siblingIds.has(slice.base) || siblingBranches.has(slice.base)) continue;
    const live = await viewPrInfo(slice.base);
    if (live?.state === "MERGED") {
      updateStackSlice(stackId, slice.id, { base: trunk });
      onLog(`external parent ${slice.base} merged (#${live.number}) — reparented ${slice.id} onto ${trunk}`);
    } else if (!live && !(await branchExists(slice.base))) {
      // No PR and no branch anywhere — the parent is gone. (A CLOSED PR or a
      // still-open parent leaves the link alone.) The `branchExists`
      // corroboration is LOAD-BEARING, not belt-and-braces: `viewPrInfo`
      // returns null for a transient gh failure exactly as it does for
      // "no PR", and without the second check a gh hiccup would reparent
      // a slice whose parent is alive. (The MERGED branch above needs no
      // such guard — a failed probe can never read as MERGED.)
      updateStackSlice(stackId, slice.id, { base: trunk });
      onLog(`external parent ${slice.base} is gone — reparented ${slice.id} onto ${trunk}`);
    }
  }
}
