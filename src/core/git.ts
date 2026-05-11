import { existsSync } from "node:fs";
import { isAbsolute, join } from "node:path";

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

/**
 * True when the given worktree has a rebase paused (rebase-merge or
 * rebase-apply dir present in its git dir). The git dir comes from
 * `git rev-parse --git-dir` so linked-worktree paths just work.
 * Used by the stack chord runner to decide between "halted on
 * conflict → escalate to claude" vs "failed for another reason →
 * just toast".
 */
export async function isRebaseInProgress(wtPath: string): Promise<boolean> {
  const r = await run(["git", "rev-parse", "--git-dir"], {
    cwd: wtPath,
    timeoutMs: 5_000,
  });
  if (r.exitCode !== 0) return false;
  const raw = r.stdout.trim();
  if (!raw) return false;
  const dir = isAbsolute(raw) ? raw : join(wtPath, raw);
  return existsSync(join(dir, "rebase-merge")) || existsSync(join(dir, "rebase-apply"));
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
