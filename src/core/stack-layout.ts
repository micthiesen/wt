/**
 * Inferred-stack layout: pure helpers that group live worktrees into
 * stacks by following their recorded fork bases (`wt new --base` →
 * wtstate `slugs[slug].baseBranch`) and lay each stack out as a tree
 * spine (connector glyph + ordinal + depth) for the worktree list.
 *
 * There is no stored stack state: a worktree whose recorded base names
 * another live worktree's branch is that worktree's child, and every
 * connected tree of two or more worktrees renders as a stack. Stack
 * identity is the root's branch name, so it shifts when the root lands
 * and is cleaned (the first child re-roots the tree) — cheap, derived,
 * and nothing depends on it being durable. No git/gh/IO here — just
 * the member list — so the TUI render path can import it freely.
 */
import { config } from "./config.ts";

/** One live worktree, as the inference input. */
export type ChainMember = {
  slug: string;
  branch: string;
  /**
   * Recorded fork base for the worktree (wtstate `baseBranch`), when
   * present. The trunk name and `undefined` both mean trunk-based.
   */
  baseBranch?: string;
};

/**
 * Position of a node within its stack's vertical spine, used to pick
 * the tree-connector glyph. Computed from the node's REAL child
 * structure (not its linear position), so a fork reads as a fork:
 *   - `single` = a standalone node (blank, no spine)
 *   - `first`  = a root with a single child (┌)
 *   - `middle` = an interior single-child link (├)
 *   - `last`   = a branch tip / leaf (└)
 *   - `fork`   = a node with ≥2 children, where the stack splits (┯)
 * The glyphs themselves live in `STACK_CONNECTOR` below. Because a
 * single gutter column can't draw a 2D tree, lane *identity* (which
 * parallel branch a row belongs to) is carried by `StackNode.lane` →
 * color, not by indentation; the glyph just marks where the split
 * happens.
 */
export type SpinePos = "single" | "first" | "middle" | "last" | "fork";

/**
 * Tree-spine connector glyph for a stack row, by the node's position.
 * Shared by the worktree list gutter and the folded-stack summary in
 * the details pane so the spine reads identically in both.
 */
export const STACK_CONNECTOR: Record<SpinePos, string> = {
  single: " ",
  first: "┌",
  middle: "├",
  last: "└",
  fork: "┯",
};

/** Canonical 2-cell ordinal label (`01`, `02`, …) used wherever a stack
 *  ordinal renders next to the connector glyph. */
export function stackOrdinalLabel(ordinal: number): string {
  return String(ordinal).padStart(2, "0").slice(0, 2);
}

export type StackNode = {
  /** Stack identity: the root member's branch. */
  stackId: string;
  slug: string;
  branch: string;
  /** 1-based display ordinal within the stack (spine order). */
  ordinal: number;
  /** Distance from the stack root (root = 0). */
  depth: number;
  pos: SpinePos;
  /**
   * Parallel-lane index → connector color. The root path is lane 0
   * (rendered dim, the "main" spine). At every fork the FIRST child
   * continues its parent's lane; each additional child opens a fresh
   * lane. A purely linear stack stays lane 0 throughout.
   */
  lane: number;
  /**
   * Branch to diff/label against. `null` for a trunk-based root; a root
   * whose recorded base names a branch with no live worktree (an
   * external ref, or a parent cleaned with the branch kept) carries
   * that branch even though it roots the spine. The render diff/sync
   * paths run a dead ref through `effectiveBaseOrTrunk` (falls back to
   * trunk at the git layer), so emitting it here is safe.
   */
  parentBranch: string | null;
  /** Display index within the stack (spine order, 0-based). */
  index: number;
};

export type StackLayout = {
  stackId: string;
  /** Nodes in display order: the spine top-to-bottom, forks linearized pre-order. */
  nodes: StackNode[];
  byBranch: Map<string, StackNode>;
};

export type StackIndexEntry = { layout: StackLayout; node: StackNode };

/**
 * Infer every stack from the live member list and build a branch →
 * (layout, node) index so the row pipeline can answer "is this worktree
 * stacked, and where does it sit?" in O(1). Layouts are returned too
 * (roots in branch order) for section headers.
 *
 * Only trees with ≥2 members become stacks — a lone worktree with a
 * recorded base (its parent isn't a live worktree) stays flat; the row
 * pipeline still shows its fork base via the per-slug record. Malformed
 * record graphs degrade gracefully: a cycle of records has no root, so
 * its members simply render as flat worktrees rather than crashing.
 */
export function buildStackIndex(members: readonly ChainMember[]): {
  byBranch: Map<string, StackIndexEntry>;
  layouts: StackLayout[];
} {
  const trunk = config.branch.base;
  const byBranchMember = new Map<string, ChainMember>();
  for (const m of members) {
    if (m.branch) byBranchMember.set(m.branch, m);
  }

  /** The member's parent member, when its recorded base names one. */
  const parentOf = (m: ChainMember): ChainMember | null => {
    if (!m.baseBranch || m.baseBranch === trunk) return null;
    if (m.baseBranch === m.branch) return null; // self-loop guard
    return byBranchMember.get(m.baseBranch) ?? null;
  };

  const children = new Map<string, ChainMember[]>();
  const roots: ChainMember[] = [];
  for (const m of byBranchMember.values()) {
    const parent = parentOf(m);
    if (!parent) {
      roots.push(m);
    } else {
      const arr = children.get(parent.branch);
      if (arr) arr.push(m);
      else children.set(parent.branch, [m]);
    }
  }
  roots.sort((a, b) => a.branch.localeCompare(b.branch));
  for (const arr of children.values()) {
    arr.sort((a, b) => a.branch.localeCompare(b.branch));
  }

  const byBranch = new Map<string, StackIndexEntry>();
  const layouts: StackLayout[] = [];
  for (const root of roots) {
    if ((children.get(root.branch) ?? []).length === 0) continue; // not a stack
    const stackId = root.branch;
    const nodes: StackNode[] = [];
    let index = 0;
    let nextLane = 0;
    // Pre-order DFS down the spine. `pos` comes from the node's real
    // child count + depth (so a fork reads as a fork, not a chain
    // link); `lane` is threaded down the spine, branching at each fork.
    const seen = new Set<string>();
    const walk = (m: ChainMember, depth: number, lane: number): void => {
      if (seen.has(m.branch)) return; // cycle guard
      seen.add(m.branch);
      const kids = children.get(m.branch) ?? [];
      const pos: SpinePos =
        kids.length >= 2
          ? "fork"
          : kids.length === 0
            ? depth === 0
              ? "single" // unreachable here (roots without kids skip), kept for shape parity
              : "last"
            : depth === 0
              ? "first"
              : "middle";
      nodes.push({
        stackId,
        slug: m.slug,
        branch: m.branch,
        ordinal: index + 1,
        depth,
        pos,
        lane,
        parentBranch:
          depth === 0
            ? m.baseBranch && m.baseBranch !== trunk && m.baseBranch !== m.branch
              ? m.baseBranch // external / dangling parent ref
              : null // trunk-based, or a nonsense self-referential record
            : m.baseBranch!,
        index: index++,
      });
      // First child stays on this node's lane; each extra child at a
      // fork opens a fresh lane so parallel siblings get distinct colors.
      kids.forEach((c, ci) => walk(c, depth + 1, ci === 0 ? lane : ++nextLane));
    };
    walk(root, 0, 0);
    const layout: StackLayout = {
      stackId,
      nodes,
      byBranch: new Map(nodes.map((n) => [n.branch, n])),
    };
    layouts.push(layout);
    for (const node of nodes) byBranch.set(node.branch, { layout, node });
  }
  return { byBranch, layouts };
}
