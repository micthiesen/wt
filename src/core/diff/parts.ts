/**
 * One file's contribution to the diff context, plus the data the
 * fit/render pipeline needs to reason about it. A `Part` starts at
 * `full` mode and gets stepped down by the greedy reducer in `fit.ts`
 * when the prompt is over budget.
 */

/**
 * Render modes ordered most → least detailed. The greedy reducer
 * walks this list one step at a time per file when budget pressure
 * forces compaction.
 */
export const MODE_ORDER = ["full", "tight", "hunks", "dropped"] as const;
export type FileMode = (typeof MODE_ORDER)[number];

export type Part = {
  /** Post-rename ("b/") path as `git diff` reports it. */
  path: string;
  /** Raw block straight out of `git diff -U3 -W ...`, including header lines. */
  raw: string;
  /** Lines added (counted from the block, robust to renames where numstat would lie). */
  adds: number;
  /** Lines removed. */
  dels: number;
  /** Current mode in the fit loop; mutated by the reducer. */
  mode: FileMode;
};

/**
 * Split a `git diff` blob into per-file `Part`s. Path comes from the
 * `b/` side of `diff --git a/X b/Y` so renames carry the new name.
 *
 * `adds`/`dels` are derived by counting `+` and `-` lines in the block
 * itself rather than running `git diff --numstat` separately. `--numstat`
 * reports rename paths as `X => Y`, which doesn't match what we parse
 * out of the block — counting in-place avoids the need to reconcile.
 */
export function parseDiff(diff: string): Part[] {
  if (!diff) return [];
  // Lookahead split: each block starts with a `diff --git ` line and
  // runs up to (but not including) the next one.
  const blocks = diff
    .split(/(?=^diff --git )/m)
    .filter((b) => b.startsWith("diff --git"));
  const out: Part[] = [];
  for (const block of blocks) {
    // Match `diff --git a/<src> b/<dst>`. Falls through silently on
    // quoted paths (those with spaces / non-ASCII) — git emits those
    // wrapped in double quotes, which is rare enough in our codebase
    // that "drop the file from the prompt" is acceptable.
    const m = block.match(/^diff --git a\/(\S+) b\/(\S+)/);
    if (!m) continue;
    const path = m[2]!;
    let adds = 0;
    let dels = 0;
    for (const line of block.split("\n")) {
      // File-header markers (`+++ b/...`, `--- a/...`) start with
      // three chars; skip them so they don't get counted as content.
      if (line.startsWith("+++") || line.startsWith("---")) continue;
      if (line.startsWith("+")) adds++;
      else if (line.startsWith("-")) dels++;
    }
    out.push({ path, raw: block, adds, dels, mode: "full" });
  }
  return out;
}
