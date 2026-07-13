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
 * Resolve the restack chain containing `branch`. A branch inside an
 * inferred stack resolves the WHOLE stack (restack is a coherence
 * operation; the branch only selects which stack). Any other live
 * worktree resolves as a single-step chain — onto its recorded parent
 * when one exists, else onto trunk — so every worktree is restackable.
 * Returns null only when the branch has no live worktree.
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

  // Standalone fallback: ANY live worktree resolves as a single-step
  // chain, so `R` / `wt restack` is the universal "get me current" verb
  // rather than a stacks-only one. A recorded base (an external parent,
  // or trunk after a reconcile reparented it) replays onto it; a
  // record-free worktree replays onto trunk — a plain rebase-on-main
  // with the same engine (conflict bail, backups, retarget). A
  // self-referential record is nonsense — treated like no record (same
  // guard `buildStackIndex` applies), INCLUDING its anchor: a sha
  // recorded against a bogus parent can't be trusted as a cut point.
  const self = members.find((m) => m.branch === branch);
  if (!self) return null;
  const rec = state.slugs[self.slug];
  const selfLoop = rec?.baseBranch === branch;
  if (selfLoop) {
    return {
      root: branch,
      steps: [
        {
          slug: self.slug,
          branch,
          parentBranch: null,
          hasRecord: false,
          worktreePath: pathByBranch.get(branch)!,
        },
      ],
    };
  }
  const parentBranch =
    !rec?.baseBranch || rec.baseBranch === config.branch.base
      ? null
      : rec.baseBranch;
  return { root: branch, steps: [toStep(self, parentBranch)] };
}
