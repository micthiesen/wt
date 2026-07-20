import { branchExists } from "../git.ts";
import { viewPrInfo } from "../github.ts";
import { setSlugBase } from "../wtstate.ts";
import { type ChainStep, type RestackChain } from "./chain.ts";
import { lockChain, STACK_BUSY, type Logger } from "./shared.ts";

/**
 * Reconcile the fork-base records of the stack containing `branch`
 * against landed reality: a member whose recorded parent has MERGED (or,
 * for a parent with no live worktree, whose branch is gone everywhere)
 * is reparented onto the nearest surviving ancestor — the landed
 * parent's own recorded parent, walking up through consecutive landings,
 * falling to trunk. The member's `baseSha` anchor is PRESERVED across
 * the reparent: the landed parent's commits sit below the anchor and are
 * excluded from the next replay by construction, which is exactly what
 * makes the squash-merge case safe. Record bookkeeping only — reads
 * GitHub/git state but never rewrites branches — so `/restack` can run
 * it on its own before deciding to replay.
 */
export async function reconcileStack(
  branch: string,
  trunk: string,
  onLog: Logger,
): Promise<Set<string>> {
  const locked = await lockChain(branch, "reconcile");
  if (locked.status === "busy") {
    onLog(`skipped reconcile of ${branch} — ${STACK_BUSY}`);
    return new Set();
  }
  if (locked.status === "gone") return new Set();
  try {
    return await reconcileStackLocked(locked.chain, trunk, onLog);
  } finally {
    for (const h of locked.handles) h.release();
  }
}

async function reconcileStackLocked(
  chain: RestackChain,
  trunk: string,
  onLog: Logger,
): Promise<Set<string>> {
  const stepByBranch = new Map<string, ChainStep>(
    chain.steps.map((s) => [s.branch, s]),
  );

  // Probe every distinct non-trunk parent's PR state in parallel.
  const parents = [
    ...new Set(
      chain.steps
        .map((s) => s.parentBranch)
        .filter((p): p is string => p !== null),
    ),
  ];
  const probed = await Promise.all(
    parents.map(async (p) => ({ parent: p, live: await viewPrInfo(p) })),
  );

  const landed = new Set<string>();
  for (const { parent, live } of probed) {
    if (live?.state === "MERGED") {
      landed.add(parent);
      onLog(`parent ${parent} merged (#${live.number})`);
      continue;
    }
    // A parent that IS a live worktree obviously still exists; the
    // gone-branch case only applies to external parents. No PR and no
    // branch anywhere — the parent is gone. (A CLOSED PR or a still-open
    // parent leaves the link alone.) The `branchExists` corroboration is
    // LOAD-BEARING, not belt-and-braces: `viewPrInfo` returns null for a
    // transient gh failure exactly as it does for "no PR", and without
    // the second check a gh hiccup would reparent a member whose parent
    // is alive.
    if (!live && !stepByBranch.has(parent) && !(await branchExists(parent))) {
      landed.add(parent);
      onLog(`parent ${parent} is gone`);
    }
  }
  if (landed.size === 0) return landed;

  for (const s of chain.steps) {
    if (s.parentBranch === null || !landed.has(s.parentBranch)) continue;
    // A member that itself landed will be cleaned; don't bother
    // rewriting its record.
    if (landed.has(s.branch)) continue;
    // Walk up through consecutively-landed ancestors: the new parent is
    // the first survivor (an in-chain parent's own recorded parent), or
    // trunk when the walk runs off the chain (external parents can't be
    // walked past — their records live in another checkout, if anywhere).
    let candidate: string | null = s.parentBranch;
    while (candidate !== null && landed.has(candidate)) {
      candidate = stepByBranch.get(candidate)?.parentBranch ?? null;
    }
    const newParent = candidate ?? trunk;
    // Reparent the RECORD, preserving the anchor. `baseSha` stays valid:
    // it still names the tip this member's own commits sit on, which is
    // what keeps the subsequent replay squash-safe.
    setSlugBase(s.slug, {
      branch: newParent,
      ...(s.baseSha ? { sha: s.baseSha } : {}),
    });
    onLog(`reparented ${s.branch} onto ${newParent}`);
  }
  return landed;
}
