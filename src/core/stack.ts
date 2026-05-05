/**
 * Detect stack relationships across worktrees from reflog intent.
 *
 * "Stacked on" is a statement of intent — *this branch was forked from
 * that one* — not something we infer from content overlap. Patch-id
 * detection went the other way and produced wrong-direction results
 * (sibling parked at our old tip looked like our parent), so we
 * abandoned it. The reflog has the user's actions verbatim and is the
 * only signal that knows direction.
 *
 * Two reflog patterns count, in order:
 *
 *   1. Most recent `reset: moving to X` — the user pointed this
 *      branch at X explicitly, regardless of what came before. If X
 *      resolves to another worktree (by branch name, by SHA, or by
 *      reachability when X has since moved), that worktree is the
 *      parent.
 *
 *   2. Fork-from rebase / creation. Walking from the oldest reflog
 *      event toward newest, take the latest `branch: Created from X`
 *      / `rebase (finish): ... onto X` / `reset: moving to X` event
 *      that occurs *before* any `commit:` entry. Once the user has
 *      committed on this branch, subsequent rebases are stack
 *      maintenance, not parent declarations. Skip targets that are
 *      reachable from trunk (rebasing onto trunk isn't a stack
 *      signal).
 *
 * PR-base is a third signal but lives one layer up in
 * `resolveStackedOn` — `detectStacks` deliberately doesn't see GitHub
 * data so the source stays self-contained.
 *
 * The diff base is always the parent's branch ref. Three-dot diff
 * against it covers `mergebase(parent, child)..child`, which is the
 * child's contribution. If the stack drifts (parent rebased ahead),
 * the diff will include some not-yet-rebased commits — that's an
 * honest "your stack is stale" signal, not a bug.
 *
 * Detection runs from `config.paths.mainClone` since all worktrees
 * share one object DB — every branch tip is reachable from any cwd.
 */
import { config } from "./config.ts";
import { gitQuiet } from "./git.ts";
import { run } from "./proc.ts";
import { createLogger } from "./logger.ts";
import type { Worktree } from "./types.ts";

const log = createLogger("stack");

const GIT_TIMEOUT_MS = 5_000;

export type StackParent = {
  /** Slug of the worktree this one is stacked on. */
  slug: string;
  /** Branch name of the parent worktree (mirrors `Worktree.branch`). */
  branch: string;
  /**
   * Ref to use for `git diff <diffBase>...HEAD`. Always the parent's
   * branch name; three-dot diff handles drift naturally.
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
 * Resolve each non-main worktree's branch tip to a SHA. Skips
 * worktrees with empty `branch` defensively. All `git rev-parse` calls
 * run in parallel against the shared object DB.
 */
async function resolveTips(worktrees: readonly Worktree[]): Promise<Tip[]> {
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
      if (sha) out.push({ slug: w.slug, sha, branch: w.branch });
    }),
  );
  return out;
}

type ReflogEntry =
  | { kind: "commit" }
  | { kind: "reset"; target: string }
  | { kind: "rebase"; target: string }
  | { kind: "branch-create"; target: string }
  | { kind: "other" };

/** Newest-first list of meaningful reflog entries on `branch`. */
async function readReflog(branch: string): Promise<ReflogEntry[]> {
  const r = await run(
    ["git", "reflog", "show", "--pretty=%gs", branch],
    { cwd: config.paths.mainClone, timeoutMs: GIT_TIMEOUT_MS },
  );
  if (r.exitCode !== 0) return [];
  return r.stdout
    .split("\n")
    .filter((l) => l.length > 0)
    .map(parseReflogLine);
}

function parseReflogLine(line: string): ReflogEntry {
  if (line.startsWith("commit:") || line.startsWith("commit (")) {
    return { kind: "commit" };
  }
  const reset = line.match(/^reset: moving to (.+)$/);
  if (reset) return { kind: "reset", target: reset[1]! };
  const rebase = line.match(/^rebase \(finish\): .+ onto ([^\s]+)$/);
  if (rebase) return { kind: "rebase", target: rebase[1]! };
  const created = line.match(/^branch: Created from (.+)$/);
  if (created) return { kind: "branch-create", target: created[1]! };
  return { kind: "other" };
}

/**
 * Resolve a reflog target string to one of `tips`, if it identifies a
 * sibling worktree.
 *
 *   1. Direct branch-name match (the common case for `reset: moving
 *      to michael/eng-...`).
 *   2. Direct tip-SHA match.
 *   3. Reachability — the target SHA lies inside some current tip's
 *      history. This covers "we rebased onto X, then X rebased ahead";
 *      X is no longer at the SHA we recorded but the worktree that
 *      *was* at X still has it as an ancestor. Multiple matches break
 *      by closeness (fewest commits between target and tip), then by
 *      slug.
 */
async function resolveTarget(
  target: string,
  tips: readonly Tip[],
): Promise<Tip | null> {
  for (const t of tips) {
    if (t.branch === target) return t;
  }
  const r = await run(["git", "rev-parse", "--verify", target], {
    cwd: config.paths.mainClone,
    timeoutMs: GIT_TIMEOUT_MS,
  });
  if (r.exitCode !== 0) return null;
  const sha = r.stdout.trim();
  if (!sha) return null;
  for (const t of tips) {
    if (t.sha === sha) return t;
  }
  const reachable: Tip[] = [];
  await Promise.all(
    tips.map(async (t) => {
      if (
        await gitQuiet(["merge-base", "--is-ancestor", sha, t.sha])
      ) {
        reachable.push(t);
      }
    }),
  );
  if (reachable.length === 0) return null;
  if (reachable.length === 1) return reachable[0]!;
  const distances = await Promise.all(
    reachable.map(async (t) => {
      const r = await run(
        ["git", "rev-list", "--count", `${sha}..${t.sha}`],
        { cwd: config.paths.mainClone, timeoutMs: GIT_TIMEOUT_MS },
      );
      const n = r.exitCode === 0 ? Number.parseInt(r.stdout.trim(), 10) : NaN;
      return { t, n: Number.isFinite(n) ? n : Number.POSITIVE_INFINITY };
    }),
  );
  distances.sort((a, b) => a.n - b.n || a.t.slug.localeCompare(b.t.slug));
  return distances[0]!.t;
}

/**
 * True iff `target` (a ref or SHA) is reachable from
 * `origin/<branch.base>` — i.e. rebasing onto it isn't a stack signal,
 * it's "freshen against trunk." Falls back to checking the local trunk
 * ref when origin isn't available.
 */
async function isOnTrunk(target: string): Promise<boolean> {
  if (
    await gitQuiet([
      "merge-base",
      "--is-ancestor",
      target,
      `origin/${config.branch.base}`,
    ])
  ) {
    return true;
  }
  return gitQuiet([
    "merge-base",
    "--is-ancestor",
    target,
    config.branch.base,
  ]);
}

async function detectParent(
  child: Tip,
  tips: readonly Tip[],
): Promise<Tip | null> {
  const others = tips.filter((t) => t.slug !== child.slug);
  if (others.length === 0) return null;

  const reflog = await readReflog(child.branch);

  // Signal 1: most recent `reset: moving to X`. Older resets are stale
  // intent; if the most recent doesn't resolve to a worktree, fall
  // through rather than digging deeper.
  for (const entry of reflog) {
    if (entry.kind === "reset") {
      if (await isOnTrunk(entry.target)) break;
      const t = await resolveTarget(entry.target, others);
      if (t) return t;
      break;
    }
  }

  // Signal 2: fork-from declaration. Walk oldest-to-newest; take the
  // latest declaration that occurs before any `commit:`. After the
  // first commit the branch is "in use"; later rebases are
  // stack-maintenance, not parent declarations.
  let pendingTarget: string | null = null;
  for (let i = reflog.length - 1; i >= 0; i--) {
    const entry = reflog[i]!;
    if (entry.kind === "commit") break;
    if (
      entry.kind === "rebase" ||
      entry.kind === "reset" ||
      entry.kind === "branch-create"
    ) {
      if (await isOnTrunk(entry.target)) {
        pendingTarget = null;
        continue;
      }
      pendingTarget = entry.target;
    }
  }
  if (pendingTarget) {
    const t = await resolveTarget(pendingTarget, others);
    if (t) return t;
  }

  return null;
}

/**
 * For each non-main worktree, identify its stack parent from reflog
 * intent. Cost: one `git reflog show` per worktree plus a handful of
 * `merge-base --is-ancestor` calls per (worktree, candidate-target)
 * resolution. Sub-second on any realistic repo.
 */
export async function detectStacks(
  worktrees: readonly Worktree[],
): Promise<StackMap> {
  const tips = await resolveTips(worktrees);
  if (tips.length === 0) return {};

  const out: StackMap = {};
  await Promise.all(
    tips.map(async (child) => {
      const parent = await detectParent(child, tips);
      if (parent) {
        out[child.slug] = {
          slug: parent.slug,
          branch: parent.branch,
          diffBase: parent.branch,
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
