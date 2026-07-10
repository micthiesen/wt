/**
 * Restack maintenance over inferred stacks. `chain.ts` resolves which
 * worktrees a restack walks (from live worktrees + fork-base records),
 * `reconcile.ts` rewrites records when parents land, `replay.ts` drives
 * the native squash-safe engine to replay members onto their (possibly
 * rewritten) parents, and `rebaseStack` is the thin
 * reconcile-then-replay convenience. The genuinely hard part (anchored
 * rebase replay) lives in `RestackEngine`.
 */
import { tryAcquireLock, type LockHandle } from "../locks.ts";
import { createLogger } from "../logger.ts";
import { retargetPrBase, viewPrInfo } from "../github.ts";

export const log = createLogger("[stack-ops]");

/** Flock slug serializing stack operations across processes. */
export const STACK_LOCK_SLUG = "__stack__";

export type Logger = (line: string) => void;

/** Error every mutator returns/logs when the stack lock can't be had. */
export const STACK_BUSY = "another wt restack operation is already running";

/**
 * Acquire the cross-process stack lock, waiting briefly for a live holder
 * to finish. Both restack mutators take this — reconcile and replay each
 * do read-records → async git/gh work → write-records-back, so two
 * unserialized writers (a CLI `wt restack` racing the TUI's `R`) would
 * silently lose whichever write lands first.
 */
export async function acquireStackLock(phase: string): Promise<LockHandle | null> {
  const deadline = Date.now() + 5_000;
  for (;;) {
    const handle = tryAcquireLock(STACK_LOCK_SLUG, "stack", { phase });
    if (handle) return handle;
    if (Date.now() >= deadline) return null;
    await Bun.sleep(250);
  }
}

/** Retarget a branch's PR base to `expectedBase` when GitHub disagrees.
 *  The PR is resolved live (no cached number exists anymore); a branch
 *  with no PR, or a PR that already left OPEN, is left alone. */
export async function retargetIfNeeded(
  branch: string,
  expectedBase: string,
  onLog: Logger,
): Promise<void> {
  const live = await viewPrInfo(branch);
  if (!live || live.state !== "OPEN" || live.baseRefName === expectedBase) return;
  const r = await retargetPrBase(live.number, expectedBase);
  if (r.ok) onLog(`  retargeted PR #${live.number} base → ${expectedBase}`);
  else onLog(`  warn: retarget PR #${live.number} base: ${r.error}`);
}
