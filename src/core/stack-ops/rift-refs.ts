/**
 * Rift-backend ref materialization for the restack engine.
 *
 * The engine is pure git and reads every ref/commit with `cwd` set to a
 * slice's worktree. Under the default `git-worktree` backend that always
 * resolves — all worktrees share the main clone's one object db. Under
 * `rift` each slice is an INDEPENDENT clone with its own object db, so
 * the refs the engine needs don't exist there unless fetched in:
 *
 *   - Pass 1 (`resolveAnchor`): `merge-base <branch> <parent>` needs the
 *     parent branch as a local ref, and the trunk anchor needs a fresh
 *     `origin/<trunk>`.
 *   - Pass 2 (`resolveNewBaseSha` → `replayStep`): the child rebases onto
 *     the parent's JUST-REPLAYED tip, a commit that exists only in the
 *     parent's clone until fetched into the child's.
 *   - Push staleness (`remoteExists` / `pushIfRemoteStale`): read the
 *     slice's `origin/<branch>`, frozen at clone time.
 *
 * These helpers mirror the create-path fetch (`backend/rift.ts`
 * `materializeBranch`): `git fetch --no-tags <path> <refspec>` pulls a
 * ref straight from a sibling clone's (or the main clone's) object store
 * into the slice's. Every call is gated on `isRiftWorktree(slicePath)`,
 * so for a git-worktree slice this is a pure early return and the
 * existing behavior is byte-for-byte unchanged. Detection is per-slice
 * (a chain can mix backends after a config flip), never `config.backend`.
 */
import { isRiftWorktree } from "../backend.ts";
import { config } from "../config.ts";
import { run } from "../proc.ts";
import type { ChainStep } from "./chain.ts";
import type { Logger } from "./shared.ts";

/**
 * Best-effort `git fetch --no-tags <sourcePath> <refspec>` into `slicePath`.
 * One refspec per call so a missing source ref (e.g. a never-pushed
 * `origin/<branch>`) can't fail an unrelated fetch in the same bundle.
 * Force (`+`) so a non-fast-forward parent tip (the common rebase case)
 * still updates the slice's copy.
 */
async function fetchRef(
  slicePath: string,
  sourcePath: string,
  refspec: string,
  onLog: Logger,
): Promise<boolean> {
  const res = await run(["git", "fetch", "--no-tags", sourcePath, refspec], {
    cwd: slicePath,
  });
  if (res.exitCode !== 0) {
    onLog(
      `  rift: fetch ${refspec} from ${sourcePath} into slice failed (continuing): ${(res.stderr || res.stdout).trim()}`,
    );
    return false;
  }
  return true;
}

/**
 * Pass-1 materialization for one slice: bring the refs `resolveAnchor`
 * and `resolveNewBaseSha` read into the rift clone. No-op under
 * git-worktree. `pathByBranch` maps an in-chain member's branch → its
 * slice path (built from the resolved chain).
 */
export async function materializeSliceRefsPreAnchor(
  step: ChainStep,
  trunk: string,
  pathByBranch: ReadonlyMap<string, string>,
  onLog: Logger,
): Promise<void> {
  if (!isRiftWorktree(step.worktreePath)) return;
  const main = config.paths.mainClone;
  // Fresh trunk + this branch's own origin ref, copied from the main
  // clone (already freshened by the run's `fetchOrigin`) — one local
  // fetch each instead of N network round-trips. `origin/<branch>` may
  // be absent for a never-pushed branch; that fetch just no-ops.
  await fetchRef(
    step.worktreePath,
    main,
    `+refs/remotes/origin/${trunk}:refs/remotes/origin/${trunk}`,
    onLog,
  );
  await fetchRef(
    step.worktreePath,
    main,
    `+refs/remotes/origin/${step.branch}:refs/remotes/origin/${step.branch}`,
    onLog,
  );
  if (step.parentBranch !== null) {
    const parentPath = pathByBranch.get(step.parentBranch);
    if (parentPath) {
      // In-chain parent: its commits live in its own clone. Fetch its
      // current (pre-replay) tip as a durable local ref so `merge-base
      // <branch> <parent>` resolves for the anchor.
      await fetchRef(
        step.worktreePath,
        parentPath,
        `+refs/heads/${step.parentBranch}:refs/heads/${step.parentBranch}`,
        onLog,
      );
    } else {
      // External parent (another stack's tip, no live slice in this
      // chain): it lives on origin. Bring its origin ref so
      // `resolveNewBaseSha`'s `origin/<parent>` fallback resolves; the
      // anchor falls back to the recorded `baseSha`.
      await fetchRef(
        step.worktreePath,
        main,
        `+refs/remotes/origin/${step.parentBranch}:refs/remotes/origin/${step.parentBranch}`,
        onLog,
      );
    }
  }
}

/**
 * Pass-2 materialization for one slice: the parent already replayed this
 * run, so its NEW tip (the SHA the child rebases onto) exists only in the
 * parent's clone. Re-fetch it into the child's local `<parent>` ref so
 * `replayStep`'s `revParse(newBase)` resolves the commit. No-op under
 * git-worktree, and a no-op for a trunk-rooted or external-parent slice
 * (nothing replayed into a sibling clone to pull).
 */
export async function materializeParentNewTip(
  step: ChainStep,
  pathByBranch: ReadonlyMap<string, string>,
  onLog: Logger,
): Promise<void> {
  if (!isRiftWorktree(step.worktreePath)) return;
  if (step.parentBranch === null) return;
  const parentPath = pathByBranch.get(step.parentBranch);
  if (!parentPath) return;
  await fetchRef(
    step.worktreePath,
    parentPath,
    `+refs/heads/${step.parentBranch}:refs/heads/${step.parentBranch}`,
    onLog,
  );
}
