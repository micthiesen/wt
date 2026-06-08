/**
 * The native squash-safe restack ENGINE. wt owns the stack manifest
 * (the truth); this is the one genuinely hard mechanical piece —
 * replaying a slice's own commits onto a rewritten parent without
 * double-applying — done in pure git, no external `stack` CLI.
 *
 * Squash-safe replay, in one line: `git rebase --onto <newParentTip>
 * <anchor> <branch>`, where `anchor` is the parent-tip SHA the slice's
 * commits were last based on (`StackSlice.baseSha`). Only `anchor..branch`
 * — the slice's OWN commits — moves; a parent that squash-merged (its
 * commit no longer present as-is on the new base) is simply excluded,
 * with no patch-id guessing. Each slice is replayed IN ITS OWN WORKTREE
 * (HEAD rebases in place), so there's no `git branch -f` on a
 * checked-out branch and thus no worktree "parking" to do.
 *
 * The engine is per-slice and stateless. Ordering, anchor resolution,
 * manifest reconcile, and PR-base retargeting live in `stack-ops.ts`;
 * cross-run serialization (the flock) is taken there too, around the
 * whole replay. The `RestackEngine` seam stays so the replay mechanism
 * can be swapped or tested in isolation.
 */
import { gitRun } from "./git.ts";

export type ReplayLogger = (line: string) => void;

/** One slice's replay request. */
export type ReplayStep = {
  branch: string;
  /** The slice's own worktree; the rebase happens here, in place. */
  worktreePath: string;
  /** Old parent-tip SHA the slice's commits sit on (the `--onto` cut point). */
  anchor: string;
  /** New parent tip to land the slice's commits on — a sha or a resolvable ref. */
  newBase: string;
};

export type ReplayOutcome =
  | { ok: true; newTip: string; newBaseSha: string; moved: boolean }
  | { ok: false; conflict: true; backupBranch: string; error: string }
  | { ok: false; conflict: false; error: string };

export interface RestackEngine {
  /**
   * Squash-safe replay of ONE slice in its worktree, then force-with-lease
   * push. A no-op (`moved: false`) when the slice already sits on `newBase`.
   * Bails clean on a cherry-pick conflict: aborts the rebase, leaves a
   * `backup/...` branch at the pre-rebase tip, and returns it so the caller
   * hands resolution to a human / skill. wt never auto-resolves conflicts.
   */
  replaySlice(step: ReplayStep, onLog: ReplayLogger): Promise<ReplayOutcome>;
}

/** Resolve a ref to its SHA in `cwd`, or null if it doesn't resolve. */
async function revParse(ref: string, cwd: string): Promise<string | null> {
  const r = await gitRun(["rev-parse", "--verify", "--quiet", `${ref}^{commit}`], cwd);
  const sha = r.stdout.trim();
  return r.exitCode === 0 && sha ? sha : null;
}

export class NativeRestackEngine implements RestackEngine {
  async replaySlice(step: ReplayStep, onLog: ReplayLogger): Promise<ReplayOutcome> {
    const { branch, worktreePath, anchor, newBase } = step;
    const newBaseSha = await revParse(newBase, worktreePath);
    if (!newBaseSha) {
      return { ok: false, conflict: false, error: `cannot resolve base ref ${newBase} for ${branch}` };
    }
    const beforeTip = await revParse(branch, worktreePath);
    if (!beforeTip) {
      return { ok: false, conflict: false, error: `cannot resolve branch ${branch}` };
    }

    // Parent tip unchanged → the slice already sits on the right base.
    // Nothing to replay; report the current tip so the chain continues.
    if (anchor === newBaseSha) {
      onLog(`  ${branch}: already on base, skipping`);
      return { ok: true, newTip: beforeTip, newBaseSha, moved: false };
    }

    // Snapshot the pre-rebase tip on a backup ref so a conflict bail (or a
    // bad force-push) is always recoverable. Kept only on conflict; deleted
    // on success so backups don't pile up.
    const backupBranch = `backup/restack-${epochMs()}-${branch}`;
    const backup = await gitRun(["branch", "--force", backupBranch, beforeTip], worktreePath);
    if (backup.exitCode !== 0) {
      return {
        ok: false,
        conflict: false,
        error: `could not snapshot ${branch} to ${backupBranch}: ${backup.stderr.trim()}`,
      };
    }

    onLog(`  rebase ${branch} onto ${short(newBaseSha)} (from ${short(anchor)})`);
    const rebase = await gitRun(["rebase", "--onto", newBaseSha, anchor, branch], worktreePath);
    if (rebase.exitCode !== 0) {
      // Abort to restore a clean tree; the backup holds the original tip.
      const detail = (rebase.stderr || rebase.stdout).trim();
      const aborted = await gitRun(["rebase", "--abort"], worktreePath);
      if (aborted.exitCode !== 0) {
        // Abort failed → the worktree is still mid-rebase, NOT clean. Don't
        // report a clean conflict bail; point the operator at the stuck tree.
        return {
          ok: false,
          conflict: false,
          error: `conflict replaying ${branch}, and \`git rebase --abort\` failed — worktree ${worktreePath} left mid-rebase (backup at ${backupBranch}); resolve it manually`,
        };
      }
      return {
        ok: false,
        conflict: true,
        backupBranch,
        error: `conflict replaying ${branch} onto ${short(newBaseSha)}${detail ? `: ${detail}` : ""}`,
      };
    }

    const newTip = await revParse(branch, worktreePath);
    if (!newTip) {
      return { ok: false, conflict: false, error: `lost ${branch} tip after rebase` };
    }

    // Already correct and unmoved by the rebase (e.g. all of the slice's
    // commits were already present on the new base): skip the push.
    if (newTip === beforeTip) {
      await deleteBackup(backupBranch, worktreePath);
      onLog(`  ${branch}: no commits to move`);
      return { ok: true, newTip, newBaseSha, moved: false };
    }

    const push = await gitRun(
      ["push", "--force-with-lease", "origin", branch],
      worktreePath,
    );
    if (push.exitCode !== 0) {
      return {
        ok: false,
        conflict: false,
        error: `push ${branch}: ${(push.stderr || push.stdout).trim()}`,
      };
    }
    await deleteBackup(backupBranch, worktreePath);
    onLog(`  pushed ${branch}`);
    return { ok: true, newTip, newBaseSha, moved: true };
  }
}

/** Best-effort delete of a backup ref after a clean replay. */
async function deleteBackup(backupBranch: string, cwd: string): Promise<void> {
  await gitRun(["branch", "-D", backupBranch], cwd);
}

/** Epoch ms as a ref-safe token. Split out so it's easy to see/replace. */
function epochMs(): number {
  return Date.now();
}

function short(sha: string): string {
  return sha.slice(0, 9);
}

/** Default engine instance — swap the constructor here to change the impl. */
export const restackEngine: RestackEngine = new NativeRestackEngine();
