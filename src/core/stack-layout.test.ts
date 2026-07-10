/**
 * Golden tests for the inferred-stack layout: `buildStackIndex` turns
 * live worktrees + fork-base records into the tree spine the TUI
 * renders AND the replay order the restack engine walks, so the edge
 * cases its docstring promises (cycles, self-loops, trunk records,
 * dangling parents, forks) are pinned here.
 */
import { expect, test } from "bun:test";

import { config } from "./config.ts";
import { buildStackIndex, type ChainMember } from "./stack-layout.ts";

const trunk = config.branch.base;

function m(slug: string, branch: string, baseBranch?: string): ChainMember {
  return baseBranch === undefined ? { slug, branch } : { slug, branch, baseBranch };
}

test("linear chain lays out root-first with 1-based ordinals and spine glyph positions", () => {
  const { byBranch, layouts } = buildStackIndex([
    m("c", "C", "B"),
    m("a", "A"),
    m("b", "B", "A"),
  ]);
  expect(layouts).toHaveLength(1);
  const nodes = layouts[0]!.nodes;
  expect(nodes.map((n) => n.branch)).toEqual(["A", "B", "C"]);
  expect(nodes.map((n) => n.ordinal)).toEqual([1, 2, 3]);
  expect(nodes.map((n) => n.depth)).toEqual([0, 1, 2]);
  expect(nodes.map((n) => n.pos)).toEqual(["first", "middle", "last"]);
  expect(nodes.map((n) => n.parentBranch)).toEqual([null, "A", "B"]);
  expect(nodes.every((n) => n.stackId === "A")).toBe(true);
  expect(byBranch.get("C")!.layout.stackId).toBe("A");
});

test("a fork renders as a fork and each extra child opens a fresh lane", () => {
  const { layouts } = buildStackIndex([
    m("a", "A"),
    m("b1", "B1", "A"),
    m("b2", "B2", "A"),
  ]);
  expect(layouts).toHaveLength(1);
  const nodes = layouts[0]!.nodes;
  expect(nodes.map((n) => n.branch)).toEqual(["A", "B1", "B2"]);
  expect(nodes[0]!.pos).toBe("fork");
  expect(nodes.map((n) => n.lane)).toEqual([0, 0, 1]);
});

test("a lone worktree (or one whose record dangles) is not a stack", () => {
  const { byBranch, layouts } = buildStackIndex([
    m("a", "A"),
    m("b", "B", "gone-branch"),
  ]);
  expect(layouts).toHaveLength(0);
  expect(byBranch.size).toBe(0);
});

test("a root based on an external branch keeps it as parentBranch; trunk records read as trunk", () => {
  const { layouts } = buildStackIndex([
    m("a", "A", "external-parent"),
    m("b", "B", "A"),
    // A trunk-valued record (post-reconcile) must not chain — C roots its
    // own would-be stack, and with no children it stays flat.
    m("c", "C", trunk),
  ]);
  expect(layouts).toHaveLength(1);
  const nodes = layouts[0]!.nodes;
  expect(nodes[0]!.branch).toBe("A");
  expect(nodes[0]!.parentBranch).toBe("external-parent");
  expect(nodes[1]!.parentBranch).toBe("A");
});

test("record cycles have no root and degrade to flat rows (including their dependents)", () => {
  const { byBranch, layouts } = buildStackIndex([
    m("a", "A", "B"),
    m("b", "B", "A"),
    m("c", "C", "A"),
  ]);
  expect(layouts).toHaveLength(0);
  expect(byBranch.size).toBe(0);
});

test("a self-referential record is ignored, so real children still chain under the branch", () => {
  const { layouts } = buildStackIndex([
    m("a", "A", "A"),
    m("b", "B", "A"),
  ]);
  expect(layouts).toHaveLength(1);
  expect(layouts[0]!.nodes.map((n) => n.branch)).toEqual(["A", "B"]);
  expect(layouts[0]!.nodes[0]!.parentBranch).toBeNull();
});

test("two independent trees index as two stacks keyed by their roots", () => {
  const { byBranch, layouts } = buildStackIndex([
    m("a", "A"),
    m("a2", "A2", "A"),
    m("z", "Z"),
    m("z2", "Z2", "Z"),
  ]);
  expect(layouts.map((l) => l.stackId).sort()).toEqual(["A", "Z"]);
  expect(byBranch.get("A2")!.layout.stackId).toBe("A");
  expect(byBranch.get("Z2")!.layout.stackId).toBe("Z");
});
