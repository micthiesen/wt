/**
 * Pure layout helpers over a stack manifest: parent resolution,
 * topological ordering, and the display layout (tree-connector + ordinal
 * + depth) the worktree list renders. No git/gh/IO — just the manifest
 * shape — so the TUI render path can import it without pulling in the
 * heavy materialization machinery in `stack-ops.ts`.
 */
import { config } from "./config.ts";
import type { StackManifest, StackSlice } from "./wtstate.ts";

/**
 * True when a slice's `base` is the configured trunk. A trunk-based slice
 * branches off `origin/<trunk>`, opens its PR against trunk, and isn't
 * tracked by the engine (which rejects trunk parents). Trunk identity
 * comes solely from `config.branch.base` — no hardcoded `"main"`, so a
 * repo whose trunk is `master`/`develop` works. Distinct from
 * `isLaneRoot`: a stack whose root is stacked on an unmerged parent PR
 * has a *forest root* whose `base` is that parent branch, NOT trunk.
 */
export function isTrunkBase(slice: StackSlice): boolean {
  return slice.base === config.branch.base;
}

/**
 * True when a slice roots its lane (no parent slice): empty `dependsOn`,
 * or `base` is trunk. Drives the layout forest roots and the per-lane
 * sync entry points. A root stacked on an external parent branch is
 * still a lane root here (it has no parent *slice*), even though its
 * `base` is non-trunk — use `isTrunkBase` for trunk-specific decisions.
 */
export function isLaneRoot(slice: StackSlice): boolean {
  return slice.dependsOn.length === 0 || isTrunkBase(slice);
}

/**
 * The branch a slice stacks on, for the diff base / PR base / engine
 * track target. `base` may be the trunk name, a sibling slice's `id`, or
 * an external branch (when the whole stack is stacked on an unmerged
 * parent PR — all three are honored):
 *   - trunk      → `config.branch.base`
 *   - slice id   → that slice's branch
 *   - else       → `base` verbatim (an external branch ref)
 */
export function resolveParentBranch(
  manifest: StackManifest,
  slice: StackSlice,
): string {
  if (isTrunkBase(slice)) return config.branch.base;
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
 * Transitive ancestors of every slice, by id, over the SAME effective-edge
 * set as `topoSortSlices`: explicit `dependsOn` PLUS `base` when it names a
 * sibling slice id (the manifest may encode the parent either way). This must
 * match the materialize parent (`resolveParentBranch`), because partial-file
 * reconstruction applies "base + ancestor-owned + own" hunks — an ancestor
 * reachable only through `base` that were missed would silently drop the
 * parent's hunks on a shared file. Shared by `applyStack` (materialize) and
 * the `apply --verify` gate so both reason about identical closures.
 */
export function transitiveAncestors(slices: StackSlice[]): Map<string, Set<string>> {
  const byId = new Map(slices.map((s) => [s.id, s]));
  const directDeps = (s: StackSlice): string[] => {
    const set = new Set(s.dependsOn.filter((d) => byId.has(d)));
    if (s.base !== s.id && byId.has(s.base)) set.add(s.base);
    return [...set];
  };
  const cache = new Map<string, Set<string>>();
  const visit = (id: string, stack: Set<string>): Set<string> => {
    const hit = cache.get(id);
    if (hit) return hit;
    const acc = new Set<string>();
    const s = byId.get(id);
    if (s && !stack.has(id)) {
      stack.add(id);
      for (const dep of directDeps(s)) {
        acc.add(dep);
        for (const a of visit(dep, stack)) acc.add(a);
      }
      stack.delete(id);
    }
    cache.set(id, acc);
    return acc;
  };
  for (const s of slices) visit(s.id, new Set());
  return cache;
}

/**
 * Position of a node within its lane's vertical spine, used to pick the
 * tree-connector glyph: `single` = a one-slice lane (blank, no spine),
 * `first` = a chain root with children (┌), `last` = a chain tip (└),
 * `middle` = a stacked link in between (├). The glyphs themselves live
 * in `STACK_CONNECTOR` below.
 */
export type SpinePos = "single" | "first" | "middle" | "last";

/**
 * Tree-spine connector glyph for a managed-stack row, by the slice's
 * position in its lane. `single` = a standalone lane (blank — no chain
 * above/below to draw, so draw nothing); `first` ┌ = chain root with
 * children; `middle` ├ = a stacked link; `last` └ = the chain tip.
 * Shared by the worktree list gutter and the folded-stack summary in
 * the details pane so the spine reads identically in both.
 */
export const STACK_CONNECTOR: Record<SpinePos, string> = {
  single: " ",
  first: "┌",
  middle: "├",
  last: "└",
};

/** Canonical 2-cell ordinal label (`01`, `02`, …) used wherever a stack
 *  ordinal renders next to the connector glyph. */
export function stackOrdinalLabel(ordinal: number): string {
  return String(ordinal).padStart(2, "0").slice(0, 2);
}

export type StackNode = {
  stackId: string;
  slice: StackSlice;
  /** 1-based stack ordinal from the manifest. */
  ordinal: number;
  /** Distance from this node's lane root (root = 0). */
  depth: number;
  pos: SpinePos;
  /**
   * Branch to diff/label against. `null` only for a trunk-based slice; an
   * external-base lane root (stack-on-stack) carries its real parent branch
   * even though it roots its section in the spine.
   */
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
  // Spine classification and the diff base are resolved SEPARATELY because
  // they answer different questions. The spine (root vs child) is about an
  // in-stack sibling parent: a slice with no sibling parent roots its
  // section — including a stack-on-stack root, whose real parent lives in
  // another stack's section and so can't be drawn under it here. The diff
  // base (`parentBranch`) is resolved via `resolveParentBranch` (a sibling-id
  // base → that slice's branch; an external base → passed through), so an
  // external-branch root still diffs/labels against its real parent
  // (e.g. the parent stack's tip) rather than degrading to trunk. The render
  // diff/sync/git-activity paths run a dead `parentBranch` through
  // `effectiveBaseOrTrunk` (falls back to trunk at the git layer), so
  // emitting a possibly-dead external ref here is safe for them; interactive
  // consumers that shell out (the F11 diff session) guard it the same way.
  const parentBranchOf = new Map<string, string | null>();
  for (const s of manifest.slices) {
    const parent = parentSliceOf(s);
    parentBranchOf.set(s.id, isTrunkBase(s) ? null : resolveParentBranch(manifest, s));
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
