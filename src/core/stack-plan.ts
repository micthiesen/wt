/**
 * Pure plan computation for the stack chord (R) operations. Takes the
 * auto-detected `stackedOn` topology from `useWorktreeRows` and emits
 * a toposorted sequence of `git rebase` steps. The TUI executes them
 * one at a time, escalating to a claude session on the first conflict.
 *
 * No I/O, no side effects — just the order + base computation.
 */

export type StackRow = {
  slug: string;
  path: string;
  branch: string;
  /** Slug of the parent worktree, when the parent is one of ours. */
  parentSlug: string | null;
  /** Parent branch name. Null when there's no detected parent. */
  parentBranch: string | null;
};

export type RebaseStep = {
  slug: string;
  path: string;
  branch: string;
  /** Ref to pass to `git rebase` — either the parent branch or trunk. */
  base: string;
};

/**
 * Order the rows for a bottom-up rebase. Each step's `base` is the
 * parent branch when one was detected; otherwise `trunkBase` (e.g.
 * `origin/main`). When a row's parent worktree is in the included
 * set, we rely on its earlier rebase having moved the local branch
 * tip — `git rebase <parentBranch>` picks that up automatically.
 *
 * Toposort uses `parentSlug` for ordering and `parentBranch` for the
 * actual ref. Edge case: a row whose parent isn't in the included
 * set is treated as a root for ordering but still rebases onto its
 * (possibly stale) parent branch, not trunk — matching user intent
 * better than silently retargeting.
 *
 * `include` filters the rows the plan covers. Omitted → all rows.
 */
export function planRebase(
  rows: readonly StackRow[],
  trunkBase: string,
  include?: (row: StackRow) => boolean,
): RebaseStep[] {
  const selected = include ? rows.filter(include) : [...rows];
  if (selected.length === 0) return [];
  const selectedSet = new Set(selected.map((r) => r.slug));
  const done = new Set<string>();
  const order: StackRow[] = [];
  // Small-N greedy toposort. Linear scans per round; for N=20 worktrees
  // it's still well under a millisecond.
  while (order.length < selected.length) {
    const next = selected.find(
      (r) =>
        !done.has(r.slug) &&
        (r.parentSlug === null ||
          !selectedSet.has(r.parentSlug) ||
          done.has(r.parentSlug)),
    );
    if (!next) break; // cycle (shouldn't happen); bail safely
    done.add(next.slug);
    order.push(next);
  }
  return order.map((r) => ({
    slug: r.slug,
    path: r.path,
    branch: r.branch,
    base: r.parentBranch ?? trunkBase,
  }));
}

/**
 * Slugs reachable from `slug` via parent (upward) and child (downward)
 * walks. Used to scope the `rebase` action to just the chain
 * containing the current worktree. Self-included; empty when the
 * slug isn't a row.
 */
export function chainOf(
  rows: readonly StackRow[],
  slug: string,
): Set<string> {
  const bySlug = new Map(rows.map((r) => [r.slug, r]));
  if (!bySlug.has(slug)) return new Set();
  const out = new Set<string>([slug]);
  // Walk up via parentSlug.
  let cur: string | null = bySlug.get(slug)!.parentSlug;
  while (cur && bySlug.has(cur) && !out.has(cur)) {
    out.add(cur);
    cur = bySlug.get(cur)!.parentSlug;
  }
  // Walk down via reverse map (BFS).
  const queue = [...out];
  while (queue.length > 0) {
    const s = queue.shift()!;
    for (const r of rows) {
      if (r.parentSlug === s && !out.has(r.slug)) {
        out.add(r.slug);
        queue.push(r.slug);
      }
    }
  }
  return out;
}
