import { statSync } from "node:fs";

import { effectiveBaseOrTrunk } from "./git.ts";
import { run } from "./proc.ts";

export type GitActivity = {
  /** Worktree directory creation time (epoch ms). null if path is gone. */
  createdMs: number | null;
  /** Most recent commit on HEAD (epoch ms). null if no commits / branch detached etc. */
  lastCommitMs: number | null;
  /** Diff stats vs the effective base: file count + lines added / removed.
   *  Effective base is `origin/<config.branch.base>` for trunk-targeted
   *  branches and the parent worktree's branch for stacked ones — same
   *  resolution rule as the AI summary diff, so the row reports the
   *  contribution of *this* PR, not parent + this combined.
   *  null if the diff command fails (e.g. base branch missing locally,
   *  worktree path gone). All zero when the branch is identical to base. */
  diff: { files: number; added: number; removed: number } | null;
};

const TIMEOUT_MS = 5000;

function createdMsFor(path: string): number | null {
  try {
    const st = statSync(path);
    return st.birthtimeMs || st.ctimeMs;
  } catch {
    return null;
  }
}

async function lastCommitMsFor(path: string): Promise<number | null> {
  const r = await run(["git", "log", "-1", "--format=%ct", "HEAD"], {
    cwd: path,
    timeoutMs: TIMEOUT_MS,
  });
  if (r.exitCode !== 0) return null;
  const secs = Number.parseInt(r.stdout.trim(), 10);
  if (!Number.isFinite(secs)) return null;
  return secs * 1000;
}

// Three-dot range: changes on HEAD not on `base`, ignoring base's own
// post-branch commits. `base` is the effective base — trunk for
// unstacked, parent branch for stacked — so the count reflects this
// branch's actual contribution.
async function diffFor(
  path: string,
  branch: string,
  base: string,
): Promise<{ files: number; added: number; removed: number } | null> {
  if (!branch) return null;
  const r = await run(
    ["git", "diff", "--shortstat", `${base}...HEAD`],
    { cwd: path, timeoutMs: TIMEOUT_MS },
  );
  if (r.exitCode !== 0) return null;
  const out = r.stdout.trim();
  if (!out) return { files: 0, added: 0, removed: 0 };
  const files = out.match(/(\d+) files? changed/);
  const added = out.match(/(\d+) insertions?\(\+\)/);
  const removed = out.match(/(\d+) deletions?\(-\)/);
  return {
    files: files ? Number.parseInt(files[1]!, 10) : 0,
    added: added ? Number.parseInt(added[1]!, 10) : 0,
    removed: removed ? Number.parseInt(removed[1]!, 10) : 0,
  };
}

export async function gitActivity(
  wt: { path: string; branch: string },
  effectiveBase?: string | null,
): Promise<GitActivity> {
  const base = await effectiveBaseOrTrunk(wt.path, effectiveBase);
  const [createdMs, lastCommitMs, diff] = await Promise.all([
    Promise.resolve(createdMsFor(wt.path)),
    lastCommitMsFor(wt.path).catch(() => null),
    diffFor(wt.path, wt.branch, base).catch(() => null),
  ]);
  return { createdMs, lastCommitMs, diff };
}
