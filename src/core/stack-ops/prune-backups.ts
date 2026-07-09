import { gitRun } from "../git.ts";
import { backupBranchOwner, backupTimestamp } from "../restack-engine.ts";
import type { Logger } from "./shared.ts";

// ---------- backup pruning ----------

export type PruneBackupsResult = { deleted: string[]; kept: string[] };

/**
 * Delete restack backup branches (`backup/restack-*` and the retired stack
 * CLI's `backup/stack-sync-*`) older than `olderThanDays` (0 = all of them).
 * Backups exist to recover an in-flight conflict bail; once a slice replays
 * clean the engine prunes its own, but conflict leftovers and pre-pruning
 * history pile up — this is the manual sweep. `git branch -D` doesn't destroy
 * commits; everything stays reachable via the reflog. Refs under `backup/`
 * that don't match a known naming scheme are left alone.
 */
export async function pruneStackBackups(
  olderThanDays: number,
  onLog: Logger,
): Promise<PruneBackupsResult> {
  const r = await gitRun(["for-each-ref", "--format=%(refname:short)", "refs/heads/backup/"]);
  const deleted: string[] = [];
  const kept: string[] = [];
  if (r.exitCode !== 0) return { deleted, kept };
  const cutoff = Date.now() - olderThanDays * 86_400_000;
  for (const ref of r.stdout.split("\n").map((l) => l.trim()).filter(Boolean)) {
    if (backupBranchOwner(ref) === null) {
      kept.push(ref);
      continue;
    }
    const ts = backupTimestamp(ref);
    if (ts === null || ts > cutoff) {
      kept.push(ref);
      continue;
    }
    const del = await gitRun(["branch", "-D", ref]);
    if (del.exitCode === 0) {
      deleted.push(ref);
      onLog(`  deleted ${ref}`);
    } else {
      kept.push(ref);
      onLog(`  could not delete ${ref}: ${(del.stderr || del.stdout).trim()}`);
    }
  }
  return { deleted, kept };
}
