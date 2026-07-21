import { isRiftWorktree } from "../backend.ts";
import { gitRun } from "../git.ts";
import { listWorktrees } from "../worktree.ts";
import { backupBranchOwner, backupTimestamp } from "./engine.ts";
import type { Logger } from "./shared.ts";

// ---------- backup pruning ----------

export type PruneBackupsResult = { deleted: string[]; kept: string[] };

/** Sweep one object store's `backup/` refs, accumulating into result. */
async function pruneBackupsIn(
  cwd: string | undefined,
  cutoff: number,
  onLog: Logger,
  out: PruneBackupsResult,
): Promise<void> {
  const args = ["for-each-ref", "--format=%(refname:short)", "refs/heads/backup/"];
  const r = cwd ? await gitRun(args, cwd) : await gitRun(args);
  if (r.exitCode !== 0) return;
  for (const ref of r.stdout.split("\n").map((l) => l.trim()).filter(Boolean)) {
    if (backupBranchOwner(ref) === null) {
      out.kept.push(ref);
      continue;
    }
    const ts = backupTimestamp(ref);
    if (ts === null || ts > cutoff) {
      out.kept.push(ref);
      continue;
    }
    const del = cwd ? await gitRun(["branch", "-D", ref], cwd) : await gitRun(["branch", "-D", ref]);
    if (del.exitCode === 0) {
      out.deleted.push(ref);
      onLog(`  deleted ${ref}${cwd ? ` (in ${cwd})` : ""}`);
    } else {
      out.kept.push(ref);
      onLog(`  could not delete ${ref}: ${(del.stderr || del.stdout).trim()}`);
    }
  }
}

/**
 * Delete restack backup branches (`backup/restack-*` and the retired stack
 * CLI's `backup/stack-sync-*`) older than `olderThanDays` (0 = all of them).
 * Backups exist to recover an in-flight conflict bail; once a branch replays
 * clean the engine prunes its own, but conflict leftovers and pre-pruning
 * history pile up — this is the manual sweep. `git branch -D` doesn't destroy
 * commits; everything stays reachable via the reflog. Refs under `backup/`
 * that don't match a known naming scheme are left alone.
 *
 * The main clone's object db is swept first (it covers every git-worktree
 * backend backup, since those worktrees share it). A rift slice is an
 * INDEPENDENT clone, so its backups live in its own object store and need a
 * per-slice sweep — the engine creates them in the slice cwd
 * (`engine.ts` `replayStep`), so the manual sweep must look there too.
 */
export async function pruneStackBackups(
  olderThanDays: number,
  onLog: Logger,
): Promise<PruneBackupsResult> {
  const result: PruneBackupsResult = { deleted: [], kept: [] };
  const cutoff = Date.now() - olderThanDays * 86_400_000;
  await pruneBackupsIn(undefined, cutoff, onLog, result);
  // Rift slices carry their own refs; sweep each independent clone too.
  const worktrees = await listWorktrees().catch(() => []);
  for (const w of worktrees) {
    if (w.isMain || !isRiftWorktree(w.path)) continue;
    await pruneBackupsIn(w.path, cutoff, onLog, result);
  }
  return result;
}
