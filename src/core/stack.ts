/**
 * Detect stack relationships across worktrees.
 *
 * Two-pass detection driven by the same goal in both passes: figure out
 * the right diff base for each worktree so the LLM summary, file stat,
 * and commit log describe what *this* PR adds — not a duplicate of
 * trunk + parent's changes.
 *
 *   Pass 1 — commit-signal. A worktree is "stacked on" another when the
 *     other's tip SHA is an ancestor of this worktree's HEAD. Cheap
 *     (`merge-base --is-ancestor`); the resulting parent's tip *is* the
 *     diff base, so three-dot diff naturally reflects the unique work.
 *
 *   Pass 2 — patch-id. Runs only for children that pass 1 missed. Uses
 *     `git cherry` to detect the case where the parent was rebased after
 *     the child branched, so the parent's *current* tip is no longer an
 *     ancestor of HEAD but its *commits* (by patch-id) still appear in
 *     HEAD's history. The diff base in this case is a SHA inside the
 *     child's history — the deepest matched commit walking from the tip
 *     — so `<diffBase>..HEAD` skips the rebased-copy commits and keeps
 *     only the child's contiguous-from-tip unique work.
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

const GIT_TIMEOUT_MS = 5_000;
const CHERRY_TIMEOUT_MS = 15_000;

/**
 * How a parent was identified.
 *
 *   "commits"  — parent's tip is an ancestor of HEAD. Stack is in sync;
 *                three-dot diff against the branch ref produces the
 *                exact unique work.
 *   "patch-id" — parent's tip is *not* an ancestor of HEAD, but parent's
 *                commits (by patch-id) appear in HEAD's history under
 *                different SHAs. Stack is out of sync (parent rebased
 *                after the child branched off, typically). The diff base
 *                is a SHA inside HEAD's history rather than a branch
 *                name; the parent's branch name is kept for UI display.
 */
export type StackVia = "commits" | "patch-id";

export type StackParent = {
  /** Slug of the worktree this one is stacked on. */
  slug: string;
  /** Branch name of the parent worktree (mirrors `Worktree.branch`). */
  branch: string;
  /** How the relationship was detected. See {@link StackVia}. */
  via: StackVia;
  /**
   * Ref to use for `git diff <diffBase>...HEAD` (and similar). Equal to
   * `branch` when `via === "commits"`; a SHA inside HEAD's history when
   * `via === "patch-id"`.
   */
  diffBase: string;
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

type Tip = { slug: string; sha: string; branch: string };

/**
 * Resolve each worktree's branch tip to a SHA. Skips the main worktree
 * (which is always trunk and would self-match every other worktree as a
 * "child" of trunk — not interesting). Skips worktrees with empty
 * `branch` defensively. All `git rev-parse` calls run in parallel
 * against the shared object DB.
 */
async function resolveTips(
  worktrees: readonly Worktree[],
): Promise<Map<string, Tip>> {
  const candidates = worktrees.filter((w) => !w.isMain && w.branch);
  const out = new Map<string, Tip>();
  await Promise.all(
    candidates.map(async (w) => {
      const r = await run(["git", "rev-parse", "--verify", w.branch], {
        cwd: config.paths.mainClone,
        timeoutMs: GIT_TIMEOUT_MS,
      });
      if (r.exitCode !== 0) return;
      const sha = r.stdout.trim();
      if (sha) out.set(w.slug, { slug: w.slug, sha, branch: w.branch });
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
async function pickClosestAncestor(
  childSha: string,
  candidates: readonly Tip[],
): Promise<Tip | null> {
  if (candidates.length === 0) return null;
  if (candidates.length === 1) return candidates[0]!;
  const ancestorOfOther = new Set<string>();
  await Promise.all(
    candidates.flatMap((a, i) =>
      candidates.map(async (b, j) => {
        if (i === j || a.sha === b.sha) return;
        if (await gitQuiet(["merge-base", "--is-ancestor", a.sha, b.sha])) {
          ancestorOfOther.add(a.slug);
        }
      }),
    ),
  );
  const deepest = candidates.filter((c) => !ancestorOfOther.has(c.slug));
  if (deepest.length === 1) return deepest[0]!;
  const distances = await Promise.all(
    deepest.map(async (c) => {
      const r = await run(
        ["git", "rev-list", "--count", `${c.sha}..${childSha}`],
        { cwd: config.paths.mainClone, timeoutMs: GIT_TIMEOUT_MS },
      );
      const n = r.exitCode === 0 ? Number.parseInt(r.stdout.trim(), 10) : NaN;
      return { c, n: Number.isFinite(n) ? n : Number.POSITIVE_INFINITY };
    }),
  );
  distances.sort((a, b) => a.n - b.n || a.c.slug.localeCompare(b.c.slug));
  return distances[0]?.c ?? null;
}

/**
 * Pass 1: for each child, find candidates whose tip is an ancestor of
 * the child's HEAD, then narrow to the immediate parent. Returns a map
 * keyed by child slug.
 */
async function detectByCommits(tips: readonly Tip[]): Promise<StackMap> {
  const out: StackMap = {};
  await Promise.all(
    tips.map(async (child) => {
      const ancestors = await Promise.all(
        tips.map(async (cand) => {
          if (cand.slug === child.slug || cand.sha === child.sha) return null;
          const isAnc = await gitQuiet([
            "merge-base",
            "--is-ancestor",
            cand.sha,
            child.sha,
          ]);
          return isAnc ? cand : null;
        }),
      );
      const parent = await pickClosestAncestor(
        child.sha,
        ancestors.filter((c): c is Tip => c !== null),
      );
      if (parent) {
        out[child.slug] = {
          slug: parent.slug,
          branch: parent.branch,
          via: "commits",
          diffBase: parent.branch,
        };
      }
    }),
  );
  return out;
}

type CherryEntry = { marker: "+" | "-"; sha: string };

/**
 * Parse `git cherry <upstream> <head>` output. Each line is `+ <sha>`
 * (commit in head whose patch-id isn't in upstream) or `- <sha>`
 * (matched). Lines come oldest-first; we preserve that order.
 */
function parseCherry(stdout: string): CherryEntry[] {
  const out: CherryEntry[] = [];
  for (const line of stdout.split("\n")) {
    const m = line.match(/^([+-]) ([0-9a-f]+)$/);
    if (m) out.push({ marker: m[1] as "+" | "-", sha: m[2]! });
  }
  return out;
}

type CherryResult = {
  matched: number;
  /**
   * SHA to use as `git diff <diffBase>...HEAD`. The deepest matched
   * (patch-id-equivalent-to-candidate) commit walking from HEAD; commits
   * above it are the child's unique-from-tip contribution. `null` when
   * no matched commit was found at or below any unique commits — caller
   * should skip this candidate.
   */
  diffBase: string | null;
};

/**
 * Score a candidate parent against a child by patch-id overlap, and
 * compute the diff base when the unique commits sit contiguously at
 * HEAD.
 *
 * `git cherry` semantics: comparing reachable-from-`merge-base..head`,
 * `+` = unique to head, `-` = patch-id present in upstream. We want the
 * count of `-` (overlap) and the deepest `-` walking from the tip. The
 * deepest one's SHA becomes the diff base, so `<diffBase>..HEAD` covers
 * just the unique-prefix-from-tip — clean for the AI summary even when
 * non-contiguous unique commits sit deeper in history.
 */
async function scoreCherry(
  child: Tip,
  candidate: Tip,
): Promise<CherryResult | null> {
  const r = await run(
    ["git", "cherry", candidate.sha, child.sha],
    { cwd: config.paths.mainClone, timeoutMs: CHERRY_TIMEOUT_MS },
  );
  if (r.exitCode !== 0) return null;
  const entries = parseCherry(r.stdout);
  let matched = 0;
  for (const e of entries) if (e.marker === "-") matched++;
  if (matched === 0) return null;
  // Walk newest-first; first matched commit is the diff base.
  let diffBase: string | null = null;
  for (let i = entries.length - 1; i >= 0; i--) {
    if (entries[i]!.marker === "-") {
      diffBase = entries[i]!.sha;
      break;
    }
  }
  return { matched, diffBase };
}

/**
 * Pass 2: for each child without a commit-signal parent, find the
 * candidate with the most patch-id overlap. Skips same-SHA peers (they
 * aren't parent/child) and any candidate that already has a
 * commit-signal parent (we want the *root* of the rebased branch, not
 * one of its descendants). Ties broken by overlap count, then slug.
 */
async function detectByPatchId(
  tips: readonly Tip[],
  resolved: StackMap,
): Promise<StackMap> {
  const orphans = tips.filter((t) => !resolved[t.slug]);
  if (orphans.length === 0) return {};
  const out: StackMap = {};
  await Promise.all(
    orphans.map(async (child) => {
      const scored = await Promise.all(
        tips.map(async (cand) => {
          if (cand.slug === child.slug || cand.sha === child.sha) return null;
          const score = await scoreCherry(child, cand);
          if (!score || !score.diffBase) return null;
          return { cand, score };
        }),
      );
      const ranked = scored
        .filter((s): s is { cand: Tip; score: CherryResult } => s !== null)
        .sort(
          (a, b) =>
            b.score.matched - a.score.matched ||
            a.cand.slug.localeCompare(b.cand.slug),
        );
      const best = ranked[0];
      if (best && best.score.diffBase) {
        out[child.slug] = {
          slug: best.cand.slug,
          branch: best.cand.branch,
          via: "patch-id",
          diffBase: best.score.diffBase,
        };
      }
    }),
  );
  return out;
}

/**
 * For each non-main worktree, identify its stack parent and the diff
 * base to use for `git diff <base>...HEAD`. Pass 1 is the cheap
 * commit-ancestry signal; pass 2 (patch-id) catches stacks that drifted
 * out of sync after a parent rebase.
 *
 * Cost model: pass 1 is O(N²) `merge-base --is-ancestor` invocations,
 * each sub-millisecond against a warm object DB. Pass 2 only touches
 * orphans (typically zero or one), and `git cherry` for a small range
 * of commits is fast. For typical N (tens of worktrees) the whole
 * thing finishes in a few hundred ms.
 */
export async function detectStacks(
  worktrees: readonly Worktree[],
): Promise<StackMap> {
  const tips = [...(await resolveTips(worktrees)).values()];
  if (tips.length === 0) return {};

  const byCommits = await detectByCommits(tips);
  const byPatchId = await detectByPatchId(tips, byCommits);
  const out: StackMap = { ...byCommits, ...byPatchId };

  const keys = Object.keys(out);
  if (keys.length > 0) {
    log.debug("detected stacks", {
      pairs: keys.map((s) => `${s}<-${out[s]!.slug}(${out[s]!.via})`),
    });
  }
  return out;
}
