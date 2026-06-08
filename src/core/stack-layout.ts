/**
 * Pure layout helpers over a stack manifest: parent resolution,
 * topological ordering, and the display layout (tree-connector + ordinal
 * + depth) the worktree list renders. No git/gh/IO — just the manifest
 * shape — so the TUI render path can import it without pulling in the
 * heavy materialization machinery in `stack-ops.ts`.
 */
import { config } from "./config.ts";
import type { StackManifest, StackSlice } from "./wtstate.ts";

/** True when a slice roots at trunk (an independent lane, no stacked parent). */
export function isLaneRoot(slice: StackSlice): boolean {
  return (
    slice.dependsOn.length === 0 ||
    slice.base === config.branch.base ||
    slice.base === "main"
  );
}

/**
 * The branch a slice stacks on. Lane roots resolve to the trunk base
 * name; stacked children resolve to the parent slice's branch. The
 * `base` field may name the parent by slice `id` or by branch; both are
 * handled.
 */
export function resolveParentBranch(
  manifest: StackManifest,
  slice: StackSlice,
): string {
  if (isLaneRoot(slice)) return config.branch.base;
  const byId = manifest.slices.find((s) => s.id === slice.base);
  if (byId) return byId.branch;
  // `base` already names a branch (or an unknown ref we pass through).
  return slice.base;
}

/**
 * Topological order over `dependsOn` (plus `base`-as-id), breaking ties
 * by ordinal. Throws on a dependency cycle or a dangling `dependsOn` id
 * so the caller can surface a clear error instead of silently dropping a
 * slice.
 */
export function topoSortSlices(manifest: StackManifest): StackSlice[] {
  const byId = new Map(manifest.slices.map((s) => [s.id, s]));
  // Effective edges = explicit `dependsOn` plus `base` when it names a
  // sibling slice by id (the manifest may encode the parent either way),
  // so a slice always sorts after the parent it'll branch from.
  const depsOf = (s: StackSlice): string[] => {
    const set = new Set(s.dependsOn);
    if (s.base !== s.id && byId.has(s.base)) set.add(s.base);
    return [...set];
  };
  for (const s of manifest.slices) {
    for (const dep of s.dependsOn) {
      if (!byId.has(dep)) {
        throw new Error(`slice ${s.id} dependsOn unknown slice ${dep}`);
      }
    }
  }
  const emitted = new Set<string>();
  const out: StackSlice[] = [];
  const remaining = [...manifest.slices].sort((a, b) => a.ordinal - b.ordinal);
  while (remaining.length > 0) {
    const idx = remaining.findIndex((s) =>
      depsOf(s).every((d) => emitted.has(d)),
    );
    if (idx === -1) {
      throw new Error(
        `dependency cycle among slices: ${remaining.map((s) => s.id).join(", ")}`,
      );
    }
    const [s] = remaining.splice(idx, 1);
    emitted.add(s!.id);
    out.push(s!);
  }
  return out;
}

/**
 * Position of a node within its lane's vertical spine, used to pick the
 * tree-connector glyph: `single` = a one-slice lane (◆), `first` = a
 * chain root with children (┌), `last` = a chain tip (└), `middle` = a
 * stacked link in between (├).
 */
export type SpinePos = "single" | "first" | "middle" | "last";

export type StackNode = {
  stackId: string;
  slice: StackSlice;
  /** 1-based stack ordinal from the manifest. */
  ordinal: number;
  /** Distance from this node's lane root (root = 0). */
  depth: number;
  pos: SpinePos;
  /** Parent branch to diff against; `null` for a lane root (trunk). */
  parentBranch: string | null;
  /** Global display index within the stack (lane order, then depth). */
  index: number;
};

export type StackLayout = {
  stackId: string;
  manifest: StackManifest;
  /** Nodes in display order: each lane's spine top-to-bottom, lanes in ordinal order. */
  nodes: StackNode[];
  byBranch: Map<string, StackNode>;
};

/**
 * Lay a manifest out as a forest of vertical spines: each lane root
 * (trunk-based slice) starts a spine, its stacked descendants follow in
 * dependency order. Returns nodes in render order with the connector
 * position + ordinal each row needs.
 *
 * Branching (a slice with multiple children) is linearized via pre-order
 * DFS; the connector glyphs approximate but the ordinals stay exact.
 *
 * This is the RENDER path: it deliberately does NOT validate (no
 * `topoSortSlices` call, no throw). A malformed manifest — a cycle, or a
 * slice whose parent resolves to nothing — degrades gracefully: the
 * affected slices fall out of `nodes`/`byBranch` and render as flat
 * worktrees rather than crashing the TUI. Validation that throws belongs
 * on the apply path (`stack-ops.applyStack`), not here.
 */
export function layoutStack(manifest: StackManifest): StackLayout {
  const byId = new Map(manifest.slices.map((s) => [s.id, s]));
  const byBranchSlice = new Map(manifest.slices.map((s) => [s.branch, s]));

  const parentSliceOf = (s: StackSlice): StackSlice | null => {
    if (isLaneRoot(s)) return null;
    const pb = resolveParentBranch(manifest, s);
    return byBranchSlice.get(pb) ?? byId.get(s.base) ?? null;
  };

  const children = new Map<string, StackSlice[]>();
  const roots: StackSlice[] = [];
  // Resolve each slice's parent slice ONCE — the spine classification
  // (root vs child) and the diff base (`parentBranch`) must agree. A
  // slice whose `base` resolves to no real sibling slice degrades to a
  // lane root (flat) rather than emitting a diff base against a ref that
  // doesn't exist.
  const parentBranchOf = new Map<string, string | null>();
  for (const s of manifest.slices) {
    const parent = parentSliceOf(s);
    parentBranchOf.set(s.id, parent ? parent.branch : null);
    if (!parent) {
      roots.push(s);
    } else {
      const arr = children.get(parent.id);
      if (arr) arr.push(s);
      else children.set(parent.id, [s]);
    }
  }
  roots.sort((a, b) => a.ordinal - b.ordinal);
  for (const arr of children.values()) arr.sort((a, b) => a.ordinal - b.ordinal);

  const nodes: StackNode[] = [];
  let index = 0;
  for (const root of roots) {
    // Pre-order DFS over this lane's spine.
    const group: { slice: StackSlice; depth: number }[] = [];
    const seen = new Set<string>();
    const walk = (s: StackSlice, depth: number): void => {
      if (seen.has(s.id)) return; // cycle guard (shouldn't happen post-topo)
      seen.add(s.id);
      group.push({ slice: s, depth });
      for (const c of children.get(s.id) ?? []) walk(c, depth + 1);
    };
    walk(root, 0);
    group.forEach(({ slice, depth }, i) => {
      const pos: SpinePos =
        group.length === 1
          ? "single"
          : i === 0
            ? "first"
            : i === group.length - 1
              ? "last"
              : "middle";
      nodes.push({
        stackId: manifest.stackId,
        slice,
        ordinal: slice.ordinal,
        depth,
        pos,
        parentBranch: parentBranchOf.get(slice.id) ?? null,
        index: index++,
      });
    });
  }
  return {
    stackId: manifest.stackId,
    manifest,
    nodes,
    byBranch: new Map(nodes.map((n) => [n.slice.branch, n])),
  };
}

export type StackIndexEntry = { layout: StackLayout; node: StackNode };

/**
 * Build a branch → (layout, node) index across every manifest so the row
 * pipeline can answer "is this worktree a managed slice, and where does
 * it sit?" in O(1). Layouts are returned too (in stackId order) for
 * section headers / progress counts.
 */
export function buildStackIndex(manifests: readonly StackManifest[]): {
  byBranch: Map<string, StackIndexEntry>;
  layouts: StackLayout[];
} {
  const byBranch = new Map<string, StackIndexEntry>();
  const layouts: StackLayout[] = [];
  for (const manifest of manifests) {
    const layout = layoutStack(manifest);
    layouts.push(layout);
    for (const node of layout.nodes) {
      byBranch.set(node.slice.branch, { layout, node });
    }
  }
  return { byBranch, layouts };
}
