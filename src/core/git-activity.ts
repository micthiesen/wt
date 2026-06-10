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
   *  Counts the WORKING TREE, not just commits: staged, unstaged, and
   *  untracked changes are folded in (the dirty flag already carries the
   *  committed-vs-not boolean; the numbers report total contribution).
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

// Diff the WORKING TREE against the fork point (explicit merge-base,
// open-ended `git diff <mb>`), not `base...HEAD` — staged and unstaged
// edits count the same as committed ones, ignoring base's own
// post-branch commits. `base` is the effective base — trunk for
// unstacked, parent branch for stacked — so the count reflects this
// branch's actual contribution. Untracked files are invisible to
// `git diff` and get folded in separately below.
async function diffFor(
  path: string,
  branch: string,
  base: string,
): Promise<{ files: number; added: number; removed: number } | null> {
  if (!branch) return null;
  const mb = await run(["git", "merge-base", base, "HEAD"], {
    cwd: path,
    timeoutMs: TIMEOUT_MS,
  });
  if (mb.exitCode !== 0) return null;
  const r = await run(
    ["git", "diff", "--shortstat", mb.stdout.trim()],
    { cwd: path, timeoutMs: TIMEOUT_MS },
  );
  if (r.exitCode !== 0) return null;
  const out = r.stdout.trim();
  const files = out.match(/(\d+) files? changed/);
  const added = out.match(/(\d+) insertions?\(\+\)/);
  const removed = out.match(/(\d+) deletions?\(-\)/);
  const untracked = await untrackedCounts(path);
  return {
    files: (files ? Number.parseInt(files[1]!, 10) : 0) + untracked.files,
    added: (added ? Number.parseInt(added[1]!, 10) : 0) + untracked.added,
    removed: removed ? Number.parseInt(removed[1]!, 10) : 0,
  };
}

/**
 * Untracked files counted as a new file whose every line is an
 * insertion — the same numbers they'll contribute once committed.
 * `--exclude-standard` keeps ignored stuff (node_modules, build
 * output) out. Line counts via one `wc -l` spawn; a missing trailing
 * newline undercounts by one, which is fine for a stats row. The wc
 * output is parsed regardless of exit code so one unreadable file
 * (dangling symlink) doesn't zero out the rest.
 */
async function untrackedCounts(
  path: string,
): Promise<{ files: number; added: number }> {
  const ls = await run(
    ["git", "ls-files", "--others", "--exclude-standard", "-z"],
    { cwd: path, timeoutMs: TIMEOUT_MS },
  );
  if (ls.exitCode !== 0) return { files: 0, added: 0 };
  const names = ls.stdout.split("\0").filter(Boolean);
  if (names.length === 0) return { files: 0, added: 0 };
  // "./" prefix so a name starting with "-" can't read as a wc flag.
  const wc = await run(["wc", "-l", ...names.map((n) => `./${n}`)], {
    cwd: path,
    timeoutMs: TIMEOUT_MS,
  });
  let added = 0;
  // Per-file lines look like "  12 ./path"; with 2+ operands wc appends
  // a "  34 total" line — drop it and sum the per-file lines.
  const lines = wc.stdout.split("\n").filter((l) => /^\s*\d+\s/.test(l));
  const perFile = names.length > 1 ? lines.slice(0, -1) : lines;
  for (const l of perFile) {
    const m = l.match(/^\s*(\d+)\s/);
    if (m) added += Number.parseInt(m[1]!, 10);
  }
  return { files: names.length, added };
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
