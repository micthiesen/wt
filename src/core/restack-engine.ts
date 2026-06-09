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
  | { ok: true; newTip: string; newBaseSha: string; moved: boolean; pushed: boolean }
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

/** A rebase failure caused by a momentary lock — the always-running TUI and
 *  prompt daemons (gitstatusd) read git across every worktree concurrently —
 *  is worth retrying. The backoff has to comfortably outlive a status sweep:
 *  immediate retries lose to the SAME lock holder every time. */
const REBASE_ATTEMPTS = 5;
const ABORT_ATTEMPTS = 4;

async function lockBackoff(attempt: number): Promise<void> {
  // Linear backoff + jitter so concurrent retriers don't re-collide in step.
  await Bun.sleep(250 * attempt + Math.floor(Math.random() * 250));
}

/** Does git stderr smell like a transient lock (index.lock / ref lock held by
 *  a concurrent reader) rather than a real failure? */
function looksLikeLockError(detail: string): boolean {
  return /unable to create '.*\.lock'|another git process/i.test(detail);
}

/**
 * `git rebase --abort` can itself lose to the same transient lock that broke
 * the pick it's cleaning up — retry it before declaring the worktree stuck.
 * The authoritative success test is `rebaseInProgress`, not the exit code.
 */
async function abortRebaseWithRetry(cwd: string): Promise<{ ok: true } | { ok: false; error: string }> {
  let lastErr = "";
  for (let attempt = 1; attempt <= ABORT_ATTEMPTS; attempt++) {
    const aborted = await gitRun(["rebase", "--abort"], cwd);
    if (!(await rebaseInProgress(cwd))) return { ok: true };
    lastErr = oneLine(aborted.stderr || aborted.stdout);
    await lockBackoff(attempt);
  }
  return { ok: false, error: lastErr };
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
    // Still push if the remote lags the local tip — the common case is a
    // hand-resolved conflict the operator rebased but forgot to push.
    if (anchor === newBaseSha) {
      const sync = await pushIfRemoteStale(branch, beforeTip, worktreePath, onLog);
      if (!sync.ok) return { ok: false, conflict: false, error: sync.error };
      if (!sync.pushed) onLog(`  ${branch}: already on base, skipping`);
      await pruneSupersededBackups(branch, worktreePath, onLog);
      return { ok: true, newTip: beforeTip, newBaseSha, moved: false, pushed: sync.pushed };
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
    // collision STOPS with a rebase in progress (conflict bail below). Two
    // other failure shapes are transient locks held by concurrent readers
    // (the TUI, gitstatusd) and get retried with backoff rather than
    // misreported: a preflight failure that never started applying, and a
    // lock that hit MID-PICK (rebase in progress but zero unmerged paths) —
    // the latter is aborted and the whole rebase re-run from scratch, which
    // is cheap because the abort restores the untouched branch tip.
    let succeeded = false;
    for (let attempt = 1; attempt <= REBASE_ATTEMPTS; attempt++) {
      const rebase = await gitRun(["rebase", "--onto", newBaseSha, anchor, branch], worktreePath);
      if (rebase.exitCode === 0) {
        succeeded = true;
        break;
      }
      const detail = oneLine(rebase.stderr || rebase.stdout);
      if (await rebaseInProgress(worktreePath)) {
        const conflicts = await gitRun(
          ["diff", "--name-only", "--diff-filter=U"],
          worktreePath,
        );
        const files = conflicts.stdout.trim().split("\n").filter(Boolean);
        const where = files.length ? ` (conflicts in ${files.join(", ")})` : detail ? `: ${detail}` : "";
        const lockMidPick = files.length === 0 && looksLikeLockError(detail);
        // Abort to restore a clean tree; the backup holds the original tip.
        // The abort can lose to the SAME lock that broke the pick — retry it.
        const aborted = await abortRebaseWithRetry(worktreePath);
        if (!aborted.ok) {
          // Abort kept failing → the worktree really is stuck mid-rebase.
          // Surface both git errors so it's actionable.
          return {
            ok: false,
            conflict: false,
            error: `rebase of ${branch} is stuck, and \`git rebase --abort\` failed ${ABORT_ATTEMPTS} times — worktree ${worktreePath} left mid-rebase (backup at ${backupBranch})${where}${aborted.error ? ` [abort: ${aborted.error}]` : ""}; resolve it manually`,
          };
        }
        if (lockMidPick && attempt < REBASE_ATTEMPTS) {
          // No conflicted paths + a lock-shaped error: not a content conflict.
          onLog(`  ${branch}: a transient lock broke the rebase mid-pick (attempt ${attempt}/${REBASE_ATTEMPTS}) — aborted clean, retrying`);
          await lockBackoff(attempt);
          continue;
        }
        if (lockMidPick) {
          await deleteBackup(backupBranch, worktreePath);
          return {
            ok: false,
            conflict: false,
            error: `could not replay ${branch} onto ${short(newBaseSha)} — a lock kept breaking the rebase across ${REBASE_ATTEMPTS} attempts (branch tip untouched)${detail ? `: ${detail}` : ""}`,
          };
        }
        // A genuine content conflict stopped the rebase (an already-upstream
        // commit is auto-dropped at exit 0, so it never lands here). The
        // conflicting-file list was captured before the abort cleared the
        // index — it's far more actionable than git's raw hint blob.
        return {
          ok: false,
          conflict: true,
          backupBranch,
          error: `conflict replaying ${branch} onto ${short(newBaseSha)}${where}`,
        };
      }
      // No rebase in progress → git never started applying; the branch tip is
      // untouched. Retry through the backoff, then surface the real git error
      // (NOT a conflict bail, NOT a stuck mid-rebase) and drop the unused
      // backup so it doesn't linger.
      if (attempt < REBASE_ATTEMPTS) {
        onLog(`  ${branch}: rebase didn't start (attempt ${attempt}/${REBASE_ATTEMPTS}, likely a transient lock) — retrying`);
        await lockBackoff(attempt);
        continue;
      }
      await deleteBackup(backupBranch, worktreePath);
      return {
        ok: false,
        conflict: false,
        error: `could not replay ${branch} onto ${short(newBaseSha)} — git rebase exited ${rebase.exitCode} without starting (branch tip untouched)${detail ? `: ${detail}` : ""}`,
      };
    }
    if (!succeeded) {
      // Unreachable — every non-zero path above returns — but keeps the
      // control flow honest for the compiler and future edits.
      await deleteBackup(backupBranch, worktreePath);
      return { ok: false, conflict: false, error: `could not replay ${branch} onto ${short(newBaseSha)}` };
    }

    const newTip = await revParse(branch, worktreePath);
    if (!newTip) {
      return { ok: false, conflict: false, error: `lost ${branch} tip after rebase` };
    }

    // Already correct and unmoved by the rebase (e.g. all of the slice's
    // commits were already present on the new base). Still sync a stale
    // remote, same as the skip path above.
    if (newTip === beforeTip) {
      await deleteBackup(backupBranch, worktreePath);
      const sync = await pushIfRemoteStale(branch, newTip, worktreePath, onLog);
      if (!sync.ok) return { ok: false, conflict: false, error: sync.error };
      if (!sync.pushed) onLog(`  ${branch}: no commits to move`);
      await pruneSupersededBackups(branch, worktreePath, onLog);
      return { ok: true, newTip, newBaseSha, moved: false, pushed: sync.pushed };
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
    await pruneSupersededBackups(branch, worktreePath, onLog);
    return { ok: true, newTip, newBaseSha, moved: true, pushed: true };
  }
}

/**
 * Push `branch` when `origin/<branch>` exists but lags the local tip — the
 * slice positionally needed no replay, but a hand-resolved conflict (or any
 * out-of-band local rebase) left the remote behind. A branch with no origin
 * ref is left alone (a planned slice has no PR yet; don't invent a remote).
 * The caller fetched origin up front, so the comparison is against live state.
 */
async function pushIfRemoteStale(
  branch: string,
  localTip: string,
  cwd: string,
  onLog: ReplayLogger,
): Promise<{ ok: true; pushed: boolean } | { ok: false; error: string }> {
  const remoteTip = await revParse(`origin/${branch}`, cwd);
  if (!remoteTip || remoteTip === localTip) return { ok: true, pushed: false };
  const push = await gitRun(["push", "--force-with-lease", "origin", branch], cwd);
  if (push.exitCode !== 0) {
    return { ok: false, error: `push ${branch} (remote was stale): ${(push.stderr || push.stdout).trim()}` };
  }
  onLog(`  pushed ${branch} (remote was stale)`);
  return { ok: true, pushed: true };
}

/**
 * The branch a backup ref snapshots, or null for a ref that doesn't match a
 * known backup naming scheme (ours `backup/restack-<epochMs>-<branch>`, or
 * the retired stack CLI's `backup/stack-sync-<isoCompact>Z-<branch>`).
 */
export function backupBranchOwner(ref: string): string | null {
  const m =
    /^backup\/restack-\d+-(.+)$/.exec(ref) ??
    /^backup\/stack-sync-\d{4}-\d{2}-\d{2}T\d+Z-(.+)$/.exec(ref);
  return m ? m[1]! : null;
}

/** When a backup ref was taken (epoch ms), parsed from its name; null when
 *  the name doesn't carry a recognizable timestamp. */
export function backupTimestamp(ref: string): number | null {
  const restack = /^backup\/restack-(\d+)-/.exec(ref);
  if (restack) return Number(restack[1]);
  const sync = /^backup\/stack-sync-(\d{4})-(\d{2})-(\d{2})T(\d{2})(\d{2})(\d{2})(\d{3})Z-/.exec(ref);
  if (!sync) return null;
  const [, y, mo, d, h, mi, s, ms] = sync;
  return Date.UTC(Number(y), Number(mo) - 1, Number(d), Number(h), Number(mi), Number(s), Number(ms));
}

/**
 * A slice that just replayed clean (tip correct, remote in sync) supersedes
 * every older backup of that branch — drop them so `git branch` stays sane.
 * Backups exist to recover an in-flight bail; once the slice lands clean
 * they're dead weight (and the commits stay reachable via the reflog anyway).
 */
async function pruneSupersededBackups(
  branch: string,
  cwd: string,
  onLog: ReplayLogger,
): Promise<void> {
  const r = await gitRun(["for-each-ref", "--format=%(refname:short)", "refs/heads/backup/"], cwd);
  if (r.exitCode !== 0) return;
  const stale = r.stdout
    .split("\n")
    .map((l) => l.trim())
    .filter((ref) => ref && backupBranchOwner(ref) === branch);
  for (const ref of stale) await gitRun(["branch", "-D", ref], cwd);
  if (stale.length > 0) onLog(`  pruned ${stale.length} stale backup(s) of ${branch}`);
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
