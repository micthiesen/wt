/**
 * Materialization + maintenance for stack manifests. wt owns the
 * manifest (truth); this module turns a planned manifest into real
 * worktrees, commits, and draft PRs (`applyStack`), reports the manifest
 * DAG against live reality (`stackStatus`), reconciles the manifest with
 * landed PRs (`reconcileStack`), and drives the native squash-safe engine
 * to replay slices onto their (possibly rewritten) parents (`replayStack`).
 * `rebaseStack` is the thin reconcile-then-replay convenience. The genuinely
 * hard part (anchored cherry-pick replay) lives in `RestackEngine`.
 */
import { tryAcquireLock, type LockHandle } from "../locks.ts";
import { createLogger } from "../logger.ts";
import { retargetPrBase, viewPrInfo } from "../github.ts";
import type { StackSlice } from "../wtstate.ts";

export const log = createLogger("[stack-ops]");

/** Flock slug serializing stack operations across processes. */
export const STACK_LOCK_SLUG = "__stack__";

export type Logger = (line: string) => void;

/** Error every mutator returns/logs when the stack lock can't be had. */
export const STACK_BUSY = "another wt stack operation is already running";

/**
 * Acquire the cross-process stack lock, waiting briefly for a live holder
 * to finish. EVERY manifest mutator takes this — not just replay. Each
 * mutator does read-manifest → async git/gh work → write-manifest-back, so
 * two unserialized writers (a CLI `wt stack apply` racing the TUI's
 * reconcile) would silently lose whichever write lands first.
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

/** Retarget a slice's PR base to `expectedBase` when GitHub disagrees. */
export async function retargetIfNeeded(
  slice: StackSlice,
  expectedBase: string,
  onLog: Logger,
): Promise<void> {
  if (!slice.pr) return;
  const live = await viewPrInfo(slice.branch);
  if (!live || live.baseRefName === expectedBase) return;
  const r = await retargetPrBase(slice.pr, expectedBase);
  if (r.ok) onLog(`  retargeted PR #${slice.pr} base → ${expectedBase}`);
  else onLog(`  warn: retarget PR #${slice.pr} base: ${r.error}`);
}
