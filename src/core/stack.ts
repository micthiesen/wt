/**
 * Detect stack relationships across worktrees.
 *
 * A worktree is "stacked on" another iff every commit the candidate
 * parent has unique past trunk (by patch-id) appears in the child's
 * history (by patch-id), and that set is non-empty. One unified rule:
 * no separate commit-ancestry vs patch-id passes.
 *
 * Why patch-id everywhere: a parent that gets rebased keeps the same
 * patches under different SHAs, so SHA-level ancestry alone produces
 * false negatives once the stack drifts. Conversely, two siblings
 * parked at the same fork point both look like ancestors by SHA but
 * have no real stacked work — patch-id-vs-trunk rejects them
 * uniformly.
 *
 * The diff base is a SHA inside the child's history — the deepest
 * patch-id-matched commit walking from HEAD, or the candidate's tip
 * itself when the stack is still SHA-aligned. `<diffBase>..HEAD` then
 * covers exactly the child's contiguous-from-tip unique work.
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

export type StackParent = {
  /** Slug of the worktree this one is stacked on. */
  slug: string;
  /** Branch name of the parent worktree (mirrors `Worktree.branch`). */
  branch: string;
  /**
   * SHA inside child's history to use as `git diff <diffBase>..HEAD`.
   * Walking from the tip, this is the deepest commit whose patch-id
   * matches one of the parent's unique-past-trunk commits — or the
   * parent's tip itself when the stack is still SHA-aligned.
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
 * (always trunk; would self-match every other worktree) and worktrees
 * whose tip hasn't diverged past trunk — those have no unique work and
 * can't be a real parent. All `git` calls run in parallel against the
 * shared object DB.
 */
async function resolveTips(
  worktrees: readonly Worktree[],
  trunkSha: string | null,
): Promise<Tip[]> {
  const candidates = worktrees.filter((w) => !w.isMain && w.branch);
  const out: Tip[] = [];
  await Promise.all(
    candidates.map(async (w) => {
      const r = await run(["git", "rev-parse", "--verify", w.branch], {
        cwd: config.paths.mainClone,
        timeoutMs: GIT_TIMEOUT_MS,
      });
      if (r.exitCode !== 0) return;
      const sha = r.stdout.trim();
      if (!sha) return;
      if (trunkSha && (await isAtOrBeforeTrunk(sha, trunkSha))) return;
      out.push({ slug: w.slug, sha, branch: w.branch });
    }),
  );
  return out;
}

/**
 * Resolve trunk to a SHA. Prefers `origin/<base>` (matches
 * `branchIsMerged` semantics elsewhere — what's been pushed defines the
 * shared trunk) and falls back to the local ref when no remote is
 * available. Returns null when neither resolves; callers treat that as
 * "skip the trunk filter and bail on detection" rather than failing
 * loudly.
 */
async function resolveTrunkSha(): Promise<string | null> {
  for (const ref of [`origin/${config.branch.base}`, config.branch.base]) {
    const r = await run(["git", "rev-parse", "--verify", ref], {
      cwd: config.paths.mainClone,
      timeoutMs: GIT_TIMEOUT_MS,
    });
    if (r.exitCode === 0) {
      const sha = r.stdout.trim();
      if (sha) return sha;
    }
  }
  return null;
}

/**
 * A branch tip is "at or before trunk" when trunk's history reaches the
 * tip — i.e. the branch has no unique commits past trunk *by SHA*. Such
 * tips are sibling branches still parked at the fork point or branches
 * that fell behind. Filter them out before scoring; they'd otherwise
 * waste a `git cherry` call only to be rejected for the same reason
 * (zero unique-past-trunk commits).
 */
async function isAtOrBeforeTrunk(
  tipSha: string,
  trunkSha: string,
): Promise<boolean> {
  return gitQuiet(["merge-base", "--is-ancestor", tipSha, trunkSha]);
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

async function gitCherry(
  upstream: string,
  head: string,
): Promise<CherryEntry[] | null> {
  const r = await run(["git", "cherry", upstream, head], {
    cwd: config.paths.mainClone,
    timeoutMs: CHERRY_TIMEOUT_MS,
  });
  if (r.exitCode !== 0) return null;
  return parseCherry(r.stdout);
}

/**
 * Tip's set of commit SHAs unique past trunk by patch-id. The `+` marks
 * in `git cherry trunk tip` — commits in `mergebase(trunk, tip)..tip`
 * whose patch-ids aren't already on trunk. Empty when the tip is fully
 * merged (every commit is a patch-id-equivalent of something on trunk).
 */
async function uniquePastTrunk(
  tipSha: string,
  trunkSha: string,
): Promise<Set<string>> {
  const entries = await gitCherry(trunkSha, tipSha);
  if (!entries) return new Set();
  return new Set(
    entries.filter((e) => e.marker === "+").map((e) => e.sha),
  );
}

type ScoreResult = {
  /** Number of candidate's unique-past-trunk commits — bigger wins. */
  matched: number;
  /** SHA in child's history; left side of `git diff <diffBase>..HEAD`. */
  diffBase: string;
};

/**
 * Score `candidate` as a possible parent of `child`.
 *
 * `git cherry <upstream> <head>` lists commits in `mergebase..head`:
 * `+` = patch-id missing from upstream, `-` = matched. The parent test
 * has three conditions:
 *
 *   1. Every candidate commit unique past trunk must be reachable from
 *      child. We see this by walking `git cherry child candidate`: any
 *      `+` entry whose SHA is in `candUnique` is candidate-only work
 *      child doesn't have, so candidate isn't a parent. Trunk-
 *      equivalent commits in candidate are filtered out — they're
 *      shared via trunk, not a stack signal — which is what makes the
 *      "1 was rebased onto new main, 2 wasn't" case still resolve to
 *      `1` instead of bailing out on trunk's `+` marks.
 *   2. Child must have its own work past mergebase. Otherwise child is
 *      identical to or a subset of candidate.
 *   3. Child's tip must be `+` (unique by patch-id). If the tip is
 *      matched, child cherry-picked candidate's tip onto its own work
 *      and there's no clean contiguous-from-tip prefix to summarise —
 *      `<diffBase>..HEAD` would be empty.
 *
 * Caveat: a sibling that cherry-picked all of candidate's commits
 * unique past trunk and added its own work would also pass; we'd pick
 * it over a real parent only on slug tie-break. Niche enough that we
 * don't try to disambiguate; the user can override via PR base.
 *
 * Diff base: deepest `-` in `git cherry candidate child` walking from
 * the tip. Condition 3 guarantees the tip is `+`, so the deepest `-`
 * sits at the boundary between unique-from-tip work (above) and
 * matched-or-older work (below). When there's no `-` at all (in-sync
 * stack — candidate's tip IS the mergebase, every commit above is
 * child's own work) the boundary is candidate's tip itself.
 */
async function scoreParent(
  child: Tip,
  candidate: Tip,
  candUnique: Set<string>,
): Promise<ScoreResult | null> {
  if (candUnique.size === 0) return null;

  // Two independent `git cherry` calls — symmetric, no shared state.
  const [childEntries, candidateEntries] = await Promise.all([
    gitCherry(candidate.sha, child.sha),
    gitCherry(child.sha, candidate.sha),
  ]);
  if (!childEntries || !candidateEntries) return null;

  // Condition 1: candidate's unique-past-trunk work all reachable from child.
  for (const e of candidateEntries) {
    if (e.marker === "+" && candUnique.has(e.sha)) return null;
  }

  // Condition 2: child has its own work past mergebase.
  let childUnique = 0;
  for (const e of childEntries) {
    if (e.marker === "+") childUnique++;
  }
  if (childUnique === 0) return null;

  // Condition 3: tip is unique.
  const tip = childEntries[childEntries.length - 1];
  if (!tip || tip.marker !== "+") return null;

  // Diff base: deepest `-` from tip; falls back to candidate's tip when
  // the stack is still SHA-aligned (childEntries is all `+`).
  let diffBase = candidate.sha;
  for (let i = childEntries.length - 2; i >= 0; i--) {
    if (childEntries[i]!.marker === "-") {
      diffBase = childEntries[i]!.sha;
      break;
    }
  }
  return { matched: candUnique.size, diffBase };
}

/**
 * For each non-main worktree, identify its stack parent via the
 * patch-id rule. Pre-computes each tip's unique-past-trunk set once
 * (shared across the N-1 pairings it appears in as a candidate).
 *
 * Cost model: N `git cherry trunk <tip>` calls up front, then 2N(N-1)
 * `git cherry` calls for the pairwise scoring — each cheap for the
 * small commit ranges typical of stacked worktrees. Typical N (tens of
 * worktrees) finishes in a few hundred ms; pathological setups could
 * take seconds.
 */
export async function detectStacks(
  worktrees: readonly Worktree[],
): Promise<StackMap> {
  const trunkSha = await resolveTrunkSha();
  if (!trunkSha) {
    log.warn("trunk ref unresolved; skipping stack detection", {
      base: config.branch.base,
    });
    return {};
  }

  const tips = await resolveTips(worktrees, trunkSha);
  if (tips.length === 0) return {};

  const uniquePerTip = new Map<string, Set<string>>();
  await Promise.all(
    tips.map(async (t) => {
      uniquePerTip.set(t.slug, await uniquePastTrunk(t.sha, trunkSha));
    }),
  );

  const out: StackMap = {};
  await Promise.all(
    tips.map(async (child) => {
      const scored = await Promise.all(
        tips.map(async (cand) => {
          if (cand.slug === child.slug || cand.sha === child.sha) return null;
          const score = await scoreParent(
            child,
            cand,
            uniquePerTip.get(cand.slug) ?? new Set(),
          );
          return score ? { cand, score } : null;
        }),
      );
      const ranked = scored
        .filter((s): s is { cand: Tip; score: ScoreResult } => s !== null)
        .sort(
          (a, b) =>
            b.score.matched - a.score.matched ||
            a.cand.slug.localeCompare(b.cand.slug),
        );
      const best = ranked[0];
      if (best) {
        out[child.slug] = {
          slug: best.cand.slug,
          branch: best.cand.branch,
          diffBase: best.score.diffBase,
        };
      }
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
