import { config } from "./config.ts";
import { run, runOk, runQuiet } from "./proc.ts";

export async function git(args: string[], cwd?: string): Promise<string> {
  return runOk(["git", ...args], { cwd: cwd ?? config.paths.mainClone });
}

export async function gitQuiet(args: string[], cwd?: string): Promise<boolean> {
  return runQuiet(["git", ...args], { cwd: cwd ?? config.paths.mainClone });
}

export async function gitRun(
  args: string[],
  cwd?: string,
) {
  return run(["git", ...args], { cwd: cwd ?? config.paths.mainClone });
}

/**
 * Resolve the effective diff/sync base for a worktree, guarding against a
 * dead parent ref. A stacked slice diffs/syncs against its parent branch;
 * once that parent merges and its worktree is cleaned (branch deleted), the
 * recorded base no longer resolves and every `<base>...HEAD` git call errors
 * out (e.g. `git rev-list` via `runOk` throws a raw `fatal: bad revision`).
 * Fall back to trunk so the row degrades to a (fat) trunk diff instead of
 * surfacing that error. `reconcileStack` is the real fix — it reparents the
 * orphan onto trunk in the manifest — so this only covers the window before
 * reconcile runs (or if the PR-merged probe hasn't landed yet). An external
 * base (stack-on-stack) still resolves, so it's left untouched.
 */
export async function effectiveBaseOrTrunk(
  wtPath: string,
  effectiveBase?: string | null,
): Promise<string> {
  const trunk = `origin/${config.branch.base}`;
  if (!effectiveBase || effectiveBase === trunk) return trunk;
  const resolves = await gitQuiet(
    ["rev-parse", "--verify", "--quiet", `${effectiveBase}^{commit}`],
    wtPath,
  );
  return resolves ? effectiveBase : trunk;
}

export async function branchExists(branch: string): Promise<boolean> {
  if (await gitQuiet(["show-ref", "--verify", "--quiet", `refs/heads/${branch}`]))
    return true;
  return gitQuiet([
    "show-ref",
    "--verify",
    "--quiet",
    `refs/remotes/origin/${branch}`,
  ]);
}

export async function branchIsGone(branch: string): Promise<boolean> {
  const r = await run(
    ["git", "for-each-ref", "--format=%(upstream:track)", `refs/heads/${branch}`],
    { cwd: config.paths.mainClone },
  );
  if (r.exitCode !== 0) return false;
  return r.stdout.trim() === "[gone]";
}

let _mainFirstParents: Promise<Set<string>> | null = null;

/**
 * SHAs on origin/main's first-parent chain. A branch tip that lives
 * here is just an older main commit (nothing was merged *from* the
 * branch); one that sits off this chain was pulled in via a real merge
 * commit.
 *
 * Cached as a promise (not a value) so concurrent callers on a cold
 * cache share a single `git rev-list` — the queryFn for every non-main
 * worktree's `branchIsMerged` calls this, and they all fire at once
 * after `invalidateMainFirstParents()`.
 */
export function mainFirstParentShas(): Promise<Set<string>> {
  if (_mainFirstParents) return _mainFirstParents;
  _mainFirstParents = (async () => {
    const r = await run(
      ["git", "rev-list", "--first-parent", `origin/${config.branch.base}`],
      { cwd: config.paths.mainClone },
    );
    return new Set(
      r.exitCode === 0 ? r.stdout.split("\n").filter(Boolean) : [],
    );
  })();
  return _mainFirstParents;
}

/** Invalidate cached first-parent set after a fetch. */
export function invalidateMainFirstParents(): void {
  _mainFirstParents = null;
}

/**
 * Subject line of the *oldest* commit on the branch since `origin/main`.
 * That's the human's "what is this work" framing — captures intent
 * before a PR exists. Returns null if the branch has no commits ahead
 * of base, or `git log` fails.
 */
export async function firstCommitSubject(wtPath: string): Promise<string | null> {
  const r = await run(
    [
      "git",
      "log",
      "--reverse",
      "--format=%s",
      `origin/${config.branch.base}..HEAD`,
    ],
    { cwd: wtPath, timeoutMs: 5_000 },
  );
  if (r.exitCode !== 0) return null;
  const first = r.stdout.split("\n").find((l) => l.length > 0);
  return first ?? null;
}

export async function branchIsMerged(branch: string): Promise<boolean> {
  // Real-divergence gate; FF-aligned branches skip out below.
  if (
    !(await gitQuiet([
      "merge-base",
      "--is-ancestor",
      branch,
      `origin/${config.branch.base}`,
    ]))
  ) {
    return false;
  }
  let branchSha: string;
  let mainSha: string;
  try {
    branchSha = await git(["rev-parse", "--verify", branch]);
    mainSha = await git(["rev-parse", "--verify", `origin/${config.branch.base}`]);
  } catch {
    return false;
  }
  if (branchSha === mainSha) return false;
  // Branch tip on main's first-parent chain = just an older main SHA
  // (branch never got its own commits). Real merge-commit merges attach
  // the branch via a second parent.
  const fps = await mainFirstParentShas();
  return !fps.has(branchSha);
}
