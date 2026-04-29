import { existsSync, readFileSync } from "node:fs";
import { join } from "node:path";

import { config } from "./config.ts";
import { git, branchIsGone, branchIsMerged, gitQuiet, gitRun } from "./git.ts";
import { lockAge, lockLabel, lockStatus } from "./locks.ts";
import { createLogger } from "./logger.ts";
import { latestLogFor } from "./logs.ts";
import { runOk, runQuiet } from "./proc.ts";
import { computeStage } from "./stage.ts";
import { type Status, StatusKind, type Worktree } from "./types.ts";

const log = createLogger("[worktree]");

export async function listWorktrees(): Promise<Worktree[]> {
  const out = await git(["worktree", "list", "--porcelain"]);
  const lines = [...out.split("\n"), ""];
  const worktrees: Worktree[] = [];
  let block: Record<string, string> = {};
  for (const line of lines) {
    if (!line) {
      if (block.worktree) {
        const path = block.worktree;
        const branch = (block.branch ?? "").replace(/^refs\/heads\//, "");
        const isMain = path === config.paths.mainClone;
        const slug = isMain ? "main" : path.split("/").pop()!;
        worktrees.push({
          path,
          branch,
          isMain,
          slug,
          stage: resolveStage(path, slug),
        });
      }
      block = {};
      continue;
    }
    const sp = line.indexOf(" ");
    if (sp === -1) block[line] = "";
    else block[line.slice(0, sp)] = line.slice(sp + 1);
  }
  return worktrees;
}

/**
 * `.sst/stage` is authoritative — pinned at create-time and used by
 * SST itself. Fall back to recomputing from the slug for worktrees
 * that haven't been initialised yet.
 */
function resolveStage(path: string, slug: string): string {
  const pinned = join(path, ".sst", "stage");
  if (existsSync(pinned)) {
    try {
      return readFileSync(pinned, "utf8").trim();
    } catch (err) {
      // .sst/stage exists but unreadable (perms, truncation mid-write) —
      // fall through to the slug-derived default.
      void err;
    }
  }
  return computeStage(slug);
}

export function isDeployed(wtPath: string): boolean {
  const outputs = join(wtPath, ".sst", "outputs.json");
  if (!existsSync(outputs)) return false;
  try {
    const data = JSON.parse(readFileSync(outputs, "utf8") || "{}");
    return typeof data === "object" && data !== null && Object.keys(data).length > 0;
  } catch {
    return false;
  }
}

export type SyncCounts = { ahead: number; behind: number };
export type SyncState = {
  /** HEAD vs origin/main — "how far has this branch diverged from base?" */
  main: SyncCounts;
  /** HEAD vs @{u} — null when the branch has no upstream (never pushed). */
  remote: SyncCounts | null;
};

async function countsFor(
  wtPath: string,
  range: string,
): Promise<SyncCounts> {
  const out = await runOk(
    ["git", "rev-list", "--left-right", "--count", range],
    { cwd: wtPath },
  );
  // Output format: `<behind>\t<ahead>`. Validate strictly so a git
  // error or malformed output surfaces as a thrown query rather than
  // silently reporting "in sync".
  const match = out.trim().match(/^(\d+)\s+(\d+)$/);
  if (!match) throw new Error(`unexpected rev-list output for ${range}: ${out}`);
  return {
    behind: Number.parseInt(match[1]!, 10),
    ahead: Number.parseInt(match[2]!, 10),
  };
}

/**
 * Ahead/behind of HEAD vs both origin/main and the branch's own
 * upstream. `remote` is null when the branch has never been pushed.
 */
export async function syncState(wtPath: string): Promise<SyncState> {
  const main = await countsFor(wtPath, `origin/${config.branch.base}...HEAD`);
  const hasUpstream = await runQuiet(
    ["git", "rev-parse", "--abbrev-ref", "@{u}"],
    { cwd: wtPath },
  );
  if (!hasUpstream) return { main, remote: null };
  const remote = await countsFor(wtPath, "@{u}...HEAD");
  return { main, remote };
}

/**
 * True when the working tree has uncommitted changes — matches git's
 * own "dirty" convention. Unpushed commits are tracked separately via
 * `syncState`; callers that want to guard against losing *any* kind of
 * work (e.g. `wt rm`) should check both.
 */
export async function worktreeIsDirty(wtPath: string): Promise<boolean> {
  const porcelain = await runOk(["git", "status", "--porcelain"], { cwd: wtPath });
  return porcelain.trim().length > 0;
}

/**
 * Count of commits on HEAD that aren't on the branch's upstream (or on
 * origin/main if there's no upstream). Used by remove flows to warn
 * about work that would be lost if the worktree is destroyed.
 */
export async function unpushedCommits(wtPath: string): Promise<number> {
  try {
    const hasUpstream = await runQuiet(
      ["git", "rev-parse", "--abbrev-ref", "@{u}"],
      { cwd: wtPath },
    );
    const ref = hasUpstream ? "@{u}..HEAD" : `origin/${config.branch.base}..HEAD`;
    const ahead = await runOk(["git", "rev-list", "--count", ref], { cwd: wtPath });
    return parseInt(ahead, 10) || 0;
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), { wtPath });
    return 0;
  }
}

export async function worktreeStatus(wt: Worktree): Promise<Status> {
  const lock = lockStatus(wt.slug);
  if (lock) {
    return {
      kind: StatusKind.Busy,
      label: lockLabel(lock),
      age: lockAge(lock) ?? undefined,
      log: latestLogFor(wt.slug) ?? undefined,
      pid: lock.pid,
      op: lock.op,
    };
  }
  if (!existsSync(wt.path)) {
    return { kind: StatusKind.Missing, label: "missing" };
  }
  if (wt.branch) {
    if (await branchIsGone(wt.branch)) {
      return { kind: StatusKind.Gone, label: "gone (squash-merged or deleted)" };
    }
    if (await branchIsMerged(wt.branch)) {
      return { kind: StatusKind.Merged, label: "merged into origin/main" };
    }
  }
  if (await worktreeIsDirty(wt.path)) {
    return { kind: StatusKind.Dirty, label: "dirty" };
  }
  return { kind: StatusKind.Clean, label: "clean" };
}

/**
 * Fetch + prune from origin, then advance local main in the main clone
 * so it tracks origin/main.
 *
 * - On main + clean → `git merge --ff-only`
 * - Not on main → `git update-ref refs/heads/main`
 * - On main + dirty → skip (origin/main is fresh, which is what the
 *   semantic checks consume; update-ref on a checked-out branch creates
 *   phantom staged changes).
 *
 * Auto-regen files (`sst-env.d.ts`) get restored before the dirty check
 * so a routine `sst deploy/delete` write doesn't push us into the skip
 * path.
 */
export async function fetchOrigin(opts: { onWarn?: (msg: string) => void } = {}): Promise<void> {
  await gitRun(["fetch", "origin", "--prune"]);
  const base = config.branch.base;
  const main = config.paths.mainClone;
  const localRef = `refs/heads/${base}`;
  const remoteRef = `origin/${base}`;
  if (!(await gitQuiet(["show-ref", "--verify", "--quiet", localRef], main))) {
    return;
  }
  if (
    !(await gitQuiet([
      "merge-base",
      "--is-ancestor",
      base,
      remoteRef,
    ], main))
  ) {
    opts.onWarn?.(
      `Local ${base} has diverged from ${remoteRef}; not updating.`,
    );
    return;
  }

  let onMain = false;
  if (await gitQuiet(["symbolic-ref", "--quiet", "HEAD"], main)) {
    const head = await git(["symbolic-ref", "--quiet", "--short", "HEAD"], main);
    onMain = head.trim() === base;
  }

  if (onMain) {
    await restoreAutoRegen(main);
    const status = await runOk(["git", "status", "--porcelain"], { cwd: main });
    if (!status.trim()) {
      await gitRun(["merge", "--ff-only", "--quiet", remoteRef], main);
    }
    // else: genuinely dirty — leave local main behind; origin/main is
    // already up-to-date from the fetch.
  } else {
    await gitRun(["update-ref", localRef, remoteRef], main);
  }
}

async function restoreAutoRegen(cwd: string): Promise<void> {
  for (const p of config.sst?.autoRegenPaths ?? []) {
    if (!existsSync(join(cwd, p))) continue;
    const porcelain = await runOk(["git", "status", "--porcelain", "--", p], { cwd });
    if (porcelain) {
      await runQuiet(["git", "checkout", "HEAD", "--", p], { cwd });
    }
  }
}
