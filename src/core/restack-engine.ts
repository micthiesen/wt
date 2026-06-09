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
import { existsSync } from "node:fs";
import { resolve as resolvePath } from "node:path";

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

/**
 * Is a rebase actually in progress in `cwd`? This is the authoritative test —
 * the presence of git's per-worktree `rebase-merge`/`rebase-apply` state dir —
 * NOT the exit code of `git rebase --abort` (which also fails when there's
 * nothing to abort, the exact ambiguity that produced false "left mid-rebase"
 * reports on slices whose rebase failed at preflight without ever starting).
 */
export async function rebaseInProgress(cwd: string): Promise<boolean> {
  for (const dir of ["rebase-merge", "rebase-apply"]) {
    const r = await gitRun(["rev-parse", "--git-path", dir], cwd);
    const p = r.stdout.trim();
    // `--git-path` is ABSOLUTE for a linked worktree (the common case here) and
    // relative to `cwd` only for the main clone. `resolvePath(cwd, p)` is
    // correct for both — Node's `resolve` returns an absolute second arg
    // unchanged and joins a relative one onto `cwd`. Don't "simplify" this.
    if (p && existsSync(resolvePath(cwd, p))) return true;
  }
  return false;
}

/** Collapse git's multi-line / `\r`-laden stderr into one clean line so it
 *  renders sanely in a one-line CLI error and a JSON log field. */
function oneLine(s: string): string {
  return s.replace(/\s+/g, " ").trim();
}

/** A preflight rebase failure (no rebase started) is often a transient lock —
 *  the always-running TUI reads git across every worktree concurrently. Retry
 *  a few times with a short backoff before giving up. */
const PREFLIGHT_ATTEMPTS = 3;

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
    // Replay the slice's own commits onto the new parent tip. A real content
    // collision STOPS with a rebase in progress (handled below). A failure
    // that leaves NO rebase in progress is a preflight error (e.g. a momentary
    // index/ref lock held by a concurrent reader) that touched nothing — retry
    // it a few times rather than misreport it as a stuck or conflicted tree.
    let rebase = await gitRun(["rebase", "--onto", newBaseSha, anchor, branch], worktreePath);
    let inProgress = rebase.exitCode === 0 ? false : await rebaseInProgress(worktreePath);
    for (let attempt = 1; rebase.exitCode !== 0 && !inProgress && attempt < PREFLIGHT_ATTEMPTS; attempt++) {
      onLog(`  ${branch}: rebase didn't start (attempt ${attempt}/${PREFLIGHT_ATTEMPTS}, likely a transient lock) — retrying`);
      await Bun.sleep(150 * attempt);
      rebase = await gitRun(["rebase", "--onto", newBaseSha, anchor, branch], worktreePath);
      inProgress = rebase.exitCode === 0 ? false : await rebaseInProgress(worktreePath);
    }
    if (rebase.exitCode !== 0) {
      const detail = oneLine(rebase.stderr || rebase.stdout);
      if (inProgress) {
        // A genuine content conflict stopped the rebase (an already-upstream
        // commit is auto-dropped at exit 0, so it never lands here). Name the
        // conflicting files before aborting — the abort clears the index, and
        // the file list is far more actionable than git's raw hint blob.
        const conflicts = await gitRun(
          ["diff", "--name-only", "--diff-filter=U"],
          worktreePath,
        );
        const files = conflicts.stdout.trim().split("\n").filter(Boolean);
        const where = files.length ? ` (conflicts in ${files.join(", ")})` : detail ? `: ${detail}` : "";
        // Abort to restore a clean tree; the backup holds the original tip.
        const aborted = await gitRun(["rebase", "--abort"], worktreePath);
        if (aborted.exitCode !== 0) {
          // Abort itself failed on an in-progress rebase → the worktree really
          // is stuck mid-rebase. Surface both git errors so it's actionable.
          const abortErr = oneLine(aborted.stderr || aborted.stdout);
          return {
            ok: false,
            conflict: false,
            error: `rebase of ${branch} is stuck, and \`git rebase --abort\` failed — worktree ${worktreePath} left mid-rebase (backup at ${backupBranch})${where}${abortErr ? ` [abort: ${abortErr}]` : ""}; resolve it manually`,
          };
        }
        return {
          ok: false,
          conflict: true,
          backupBranch,
          error: `conflict replaying ${branch} onto ${short(newBaseSha)}${where}`,
        };
      }
      // No rebase in progress → git never started applying. The branch tip is
      // untouched; this is NOT a conflict bail and NOT a stuck mid-rebase.
      // Surface the real git error (retryable) and drop the unused backup so it
      // doesn't linger.
      await deleteBackup(backupBranch, worktreePath);
      return {
        ok: false,
        conflict: false,
        error: `could not replay ${branch} onto ${short(newBaseSha)} — git rebase exited ${rebase.exitCode} without starting (branch tip untouched)${detail ? `: ${detail}` : ""}`,
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
