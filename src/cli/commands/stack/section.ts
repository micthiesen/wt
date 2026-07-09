import { getStackManifest } from "../../../core/wtstate.ts";
import { red } from "../../colors.ts";

/**
 * Print the static PR-body "Stack" section for one slice (folds
 * `split/scripts/stack-section.{sh,py}`). Reads the manifest directly (no
 * gh round-trip): a linear stack renders as the flat ordinal list, a fork
 * as a nested bullet tree. Bare `#refs` only, so GitHub keeps the merge
 * status live and the section never needs maintaining. The leading blank
 * line before `---` is load-bearing (keeps a flush prose join from turning
 * the last paragraph into a setext H2).
 */
export async function runSection(argv: string[]): Promise<number> {
  const [stackId, thisRef, ...labelParts] = argv;
  if (!stackId || !thisRef) {
    console.error(red("usage: wt stack section <stackId> <sliceIdOrPr> [label]"));
    return 2;
  }
  const manifest = getStackManifest(stackId);
  if (!manifest) {
    console.error(red(`no stack manifest: ${stackId}`));
    return 1;
  }
  const label = labelParts.join(" ") || stackId;
  const slices = [...manifest.slices].sort((a, b) => a.ordinal - b.ordinal);
  // Match a slice by its id or PR number; tolerate a `#`-prefixed PR arg.
  const wantPr = thisRef.replace(/^#/, "");
  const isThis = (s: (typeof slices)[number]): boolean =>
    s.id === thisRef || String(s.pr) === wantPr;
  const ref = (s: (typeof slices)[number]): string => (s.pr ? `#${s.pr}` : s.branch);

  // Build the slice tree from `base` ALONE (id or branch), deliberately NOT via
  // `layoutStack` (which also follows `dependsOn`). This mirrors the original
  // `stack-section.py` exactly so existing PR bodies stay byte-identical; the
  // PR-body tree and the TUI/status tree are intentionally separate renderers.
  // `base` is trunk, a sibling id, a sibling branch, or an external branch
  // (stack-on-stack root). In-stack parents resolve by id or branch; anything
  // else makes the slice a root.
  const byId = new Map(slices.map((s) => [s.id, s]));
  const byBranch = new Map(slices.map((s) => [s.branch, s]));
  const children = new Map<string, typeof slices>();
  const roots: typeof slices = [];
  for (const s of slices) {
    const parent = byId.get(s.base) ?? byBranch.get(s.base);
    if (parent && parent !== s) {
      const arr = children.get(parent.id) ?? [];
      arr.push(s);
      children.set(parent.id, arr);
    } else {
      roots.push(s);
    }
  }
  // Linear = one root, no slice with two children: rendered as the flat list.
  // `roots.length === 0` only happens on a malformed base cycle; fall back to
  // flat rather than render nothing.
  const linear = roots.length === 1 && [...children.values()].every((c) => c.length <= 1);

  const out: string[] = ["", "---", "", `Stack: **${label}**`, ""];
  if (linear || roots.length === 0) {
    for (const s of slices) {
      out.push(`${s.ordinal}. ${ref(s)}${isThis(s) ? " 👈" : ""}`);
    }
  } else {
    const seen = new Set<string>();
    const emit = (s: (typeof slices)[number], depth: number): void => {
      if (seen.has(s.id)) return;
      seen.add(s.id);
      out.push(`${"  ".repeat(depth)}- ${ref(s)}${isThis(s) ? " 👈" : ""}`);
      for (const c of (children.get(s.id) ?? []).sort((a, b) => a.ordinal - b.ordinal)) {
        emit(c, depth + 1);
      }
    };
    for (const r of roots) emit(r, 0);
    out.push("");
    out.push("*(nesting = stacks on, siblings = parallel)*");
  }
  console.log(out.join("\n"));
  return 0;
}
