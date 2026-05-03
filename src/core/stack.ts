/**
 * Detect stack relationships across worktrees by walking commit ancestry.
 *
 * A worktree is "stacked on" another when the other's branch tip is an
 * ancestor of this worktree's HEAD — i.e. the work in this worktree was
 * literally built on top of commits from the other branch. That's the
 * primary signal driving:
 *
 *   - the diff base for this worktree (so the LLM summary, file stat,
 *     and commit log all describe what this PR adds *on top of* its
 *     parent, not a duplicate of trunk + parent's changes);
 *   - the visual "↑" hint in the worktree list when a stacked worktree
 *     is placed immediately below its parent in manual order.
 *
 * Detection runs from `config.paths.mainClone` since all worktrees share
 * one object DB — every branch tip is reachable from any cwd.
 */
import { config } from "./config.ts";
import { gitQuiet } from "./git.ts";
import { run } from "./proc.ts";
import { createLogger } from "./logger.ts";
import type { Worktree } from "./types.ts";

const log = createLogger("stack");

export type StackParent = {
  /** Slug of the worktree this one is stacked on. */
  slug: string;
  /** Branch name of the parent worktree (mirrors `Worktree.branch`). */
  branch: string;
};

/**
 * Slug → parent record. Only worktrees that have a detected parent
 * appear; trunk-only branches are absent (consumers default to trunk).
 *
 * Plain object (not `Map`) so it survives JSON serialisation through
 * the TanStack Query SQLite persister — `JSON.stringify(new Map())` is
 * `"{}"`, which would silently strip the data on cache rehydrate.
 */
export type StackMap = Record<string, StackParent>;

/**
 * Resolve each worktree's branch tip to a SHA. Skips the main worktree
 * (which is always trunk and would self-match every other worktree as a
 * "child" of trunk — not interesting). Skips worktrees with empty
 * `branch` defensively. All `git rev-parse` calls run in parallel
 * against the shared object DB.
 */
async function resolveTips(
  worktrees: readonly Worktree[],
): Promise<Map<string, { sha: string; branch: string }>> {
  const candidates = worktrees.filter((w) => !w.isMain && w.branch);
  const out = new Map<string, { sha: string; branch: string }>();
  await Promise.all(
    candidates.map(async (w) => {
      const r = await run(["git", "rev-parse", "--verify", w.branch], {
        cwd: config.paths.mainClone,
        timeoutMs: 5_000,
      });
      if (r.exitCode !== 0) return;
      const sha = r.stdout.trim();
      if (sha) out.set(w.slug, { sha, branch: w.branch });
    }),
  );
  return out;
}

/**
 * Pick the *immediate* parent from a set of ancestor candidates.
 *
 * In a stack A → B → C, both A's tip and B's tip are ancestors of C, but
 * only B is the immediate parent. Filter out any candidate whose tip is
 * also an ancestor of another candidate's tip, leaving the deepest layer.
 *
 * If the filter still leaves more than one (rare — would mean two
 * branches that aren't in each other's ancestry are both ancestors of
 * the child, e.g. octopus history), pick the one with the fewest commits
 * between its tip and the child's HEAD. Ties broken by slug for
 * determinism.
 */
async function pickClosestParent(
  childSha: string,
  candidates: ReadonlyArray<{ slug: string; sha: string; branch: string }>,
): Promise<{ slug: string; branch: string } | null> {
  if (candidates.length === 0) return null;
  if (candidates.length === 1) {
    const c = candidates[0]!;
    return { slug: c.slug, branch: c.branch };
  }
  // Drop any candidate whose tip is an ancestor of another candidate —
  // those are upstream of a closer ancestor, not the immediate parent.
  const ancestorOfOther = new Set<string>();
  await Promise.all(
    candidates.flatMap((a, i) =>
      candidates.map(async (b, j) => {
        if (i === j || a.sha === b.sha) return;
        const aIsAncestorOfB = await gitQuiet([
          "merge-base",
          "--is-ancestor",
          a.sha,
          b.sha,
        ]);
        if (aIsAncestorOfB) ancestorOfOther.add(a.slug);
      }),
    ),
  );
  const deepest = candidates.filter((c) => !ancestorOfOther.has(c.slug));
  if (deepest.length === 1) {
    const c = deepest[0]!;
    return { slug: c.slug, branch: c.branch };
  }
  // Multiple deepest: pick the one with the fewest commits between its
  // tip and the child. Closer = stronger parent signal.
  const distances = await Promise.all(
    deepest.map(async (c) => {
      const r = await run(
        ["git", "rev-list", "--count", `${c.sha}..${childSha}`],
        { cwd: config.paths.mainClone, timeoutMs: 5_000 },
      );
      const n = r.exitCode === 0 ? Number.parseInt(r.stdout.trim(), 10) : NaN;
      return { c, n: Number.isFinite(n) ? n : Number.POSITIVE_INFINITY };
    }),
  );
  distances.sort((a, b) => a.n - b.n || a.c.slug.localeCompare(b.c.slug));
  const best = distances[0]?.c;
  return best ? { slug: best.slug, branch: best.branch } : null;
}

/**
 * For each non-main worktree, find the closest other worktree whose tip
 * is an ancestor of this worktree's HEAD. Returns a map keyed by slug.
 *
 * Cost model: O(N) tip resolves + O(N²) ancestor checks, all parallel.
 * Each `merge-base --is-ancestor` is sub-millisecond against a warm
 * object DB; for typical N (tens of worktrees) the whole thing finishes
 * in a few hundred ms. Optimise later if it shows up in profiles.
 */
export async function detectStacks(
  worktrees: readonly Worktree[],
): Promise<StackMap> {
  const tips = await resolveTips(worktrees);
  const entries = [...tips.entries()].map(([slug, t]) => ({
    slug,
    sha: t.sha,
    branch: t.branch,
  }));

  const out: StackMap = {};
  await Promise.all(
    entries.map(async (child) => {
      // For every other worktree, check if its tip is an ancestor of the
      // child's HEAD (and isn't the same SHA — a different worktree on
      // the same SHA isn't a parent, it's a peer).
      const ancestors = await Promise.all(
        entries.map(async (cand) => {
          if (cand.slug === child.slug) return null;
          if (cand.sha === child.sha) return null;
          const isAnc = await gitQuiet([
            "merge-base",
            "--is-ancestor",
            cand.sha,
            child.sha,
          ]);
          return isAnc ? cand : null;
        }),
      );
      const candidates = ancestors.filter(
        (c): c is { slug: string; sha: string; branch: string } => c !== null,
      );
      const parent = await pickClosestParent(child.sha, candidates);
      if (parent) out[child.slug] = parent;
    }),
  );

  const keys = Object.keys(out);
  if (keys.length > 0) {
    log.debug("detected stacks", {
      pairs: keys.map((s) => `${s}<-${out[s]!.slug}`),
    });
  }
  return out;
}
