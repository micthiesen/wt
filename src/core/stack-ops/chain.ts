/**
 * Chain resolution for the restack engine: turn a target branch into
 * the ordered list of worktrees the replay walks, straight from live
 * worktrees + per-slug fork-base records (no stored stack state).
 */
import { config } from "../config.ts";
import { buildStackIndex, type ChainMember } from "../stack-layout.ts";
import { listWorktrees } from "../worktree.ts";
import { readWtState } from "../wtstate.ts";

export type ChainStep = {
  slug: string;
  branch: string;
  /** Parent branch to replay onto. `null` = trunk. */
  parentBranch: string | null;
  /**
   * Recorded squash-safe anchor (wtstate `baseSha`): the parent-tip SHA
   * this branch's own commits sit on. Absent when never recorded;
   * replay falls back to a live merge-base then.
   */
  baseSha?: string;
  /** True when the slug carries a fork-base record at all — gates
   *  whether replay writes the advanced anchor back. */
  hasRecord: boolean;
  worktreePath: string;
};

export type RestackChain = {
  /** The stack's root branch (its display identity). */
  root: string;
  /** Steps in replay order — every parent before its children. */
  steps: ChainStep[];
};

/**
 * Resolve the stack containing `branch`. A branch inside an inferred
 * stack resolves the WHOLE stack (restack is a coherence operation; the
 * branch only selects which stack). A standalone worktree with a
 * recorded base — its parent isn't a live worktree — resolves as a
 * single-step chain so it can still replay onto its recorded parent (or
 * trunk, after a reconcile reparented it). Returns null when the branch
 * has no live worktree, or has neither a record nor dependents.
 */
export async function resolveChain(branch: string): Promise<RestackChain | null> {
  const state = readWtState();
  const worktrees = (await listWorktrees()).filter((w) => !w.isMain && w.branch);
  const members: ChainMember[] = worktrees.map((w) => ({
    slug: w.slug,
    branch: w.branch,
    baseBranch: state.slugs[w.slug]?.baseBranch,
  }));
  const pathByBranch = new Map(worktrees.map((w) => [w.branch, w.path] as const));
  const toStep = (
    m: { slug: string; branch: string },
    parentBranch: string | null,
  ): ChainStep => {
    const rec = state.slugs[m.slug];
    return {
      slug: m.slug,
      branch: m.branch,
      parentBranch,
      ...(rec?.baseSha ? { baseSha: rec.baseSha } : {}),
      hasRecord: !!rec?.baseBranch,
      worktreePath: pathByBranch.get(m.branch)!,
    };
  };

  const { byBranch } = buildStackIndex(members);
  const entry = byBranch.get(branch);
  if (entry) {
    // layout.nodes are pre-order DFS — parents always precede children.
    return {
      root: entry.layout.stackId,
      steps: entry.layout.nodes.map((n) => toStep(n, n.parentBranch)),
    };
  }

  // Standalone fallback: a worktree with a record but no live parent
  // worktree and no dependents. A self-referential record is nonsense —
  // treat it like no record (same guard `buildStackIndex` applies).
  const self = members.find((m) => m.branch === branch);
  if (!self) return null;
  const rec = state.slugs[self.slug];
  if (!rec?.baseBranch || rec.baseBranch === branch) return null;
  const parentBranch =
    rec.baseBranch === config.branch.base ? null : rec.baseBranch;
  return { root: branch, steps: [toStep(self, parentBranch)] };
}
