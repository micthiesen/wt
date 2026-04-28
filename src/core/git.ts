import { BASE_BRANCH, MAIN_CLONE } from "./paths.ts";
import { run, runOk, runQuiet } from "./proc.ts";

export async function git(args: string[], cwd?: string): Promise<string> {
  return runOk(["git", ...args], { cwd: cwd ?? MAIN_CLONE });
}

export async function gitQuiet(args: string[], cwd?: string): Promise<boolean> {
  return runQuiet(["git", ...args], { cwd: cwd ?? MAIN_CLONE });
}

export async function gitRun(
  args: string[],
  cwd?: string,
) {
  return run(["git", ...args], { cwd: cwd ?? MAIN_CLONE });
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
    { cwd: MAIN_CLONE },
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
      ["git", "rev-list", "--first-parent", `origin/${BASE_BRANCH}`],
      { cwd: MAIN_CLONE },
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

export async function branchIsMerged(branch: string): Promise<boolean> {
  // Real-divergence gate; FF-aligned branches skip out below.
  if (
    !(await gitQuiet([
      "merge-base",
      "--is-ancestor",
      branch,
      `origin/${BASE_BRANCH}`,
    ]))
  ) {
    return false;
  }
  let branchSha: string;
  let mainSha: string;
  try {
    branchSha = await git(["rev-parse", "--verify", branch]);
    mainSha = await git(["rev-parse", "--verify", `origin/${BASE_BRANCH}`]);
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
