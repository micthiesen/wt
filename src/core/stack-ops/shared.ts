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
import { resolveChain, type RestackChain } from "./chain.ts";

export const log = createLogger("[stack-ops]");

export type Logger = (line: string) => void;

/** Error every mutator returns/logs when the chain's locks can't be had. */
export const STACK_BUSY =
  "another wt operation is already running on this stack's worktrees";

export type ChainLockResult =
  | { status: "ok"; chain: RestackChain; handles: readonly LockHandle[] }
  /** Some member's per-slug lock is held by another operation. */
  | { status: "busy" }
  /** The branch resolves no live worktree — nothing to lock or restack. */
  | { status: "gone" };

/**
 * Resolve the chain containing `branch` and acquire the per-slug lock of
 * EVERY member, waiting briefly for live holders to finish. This is the
 * restack serialization boundary, scoped to one chain: two restacks of
 * disjoint chains run concurrently, while two writers touching the same
 * worktrees (a CLI `wt restack` racing the TUI's `R`, or a destroy racing
 * a replay — destroys take the same per-slug lock) still exclude each
 * other. Both restack mutators go through this — reconcile and replay
 * each do read-records → async git/gh work → write-records-back, so two
 * unserialized same-chain writers would silently lose whichever write
 * lands first.
 *
 * Deadlock-free by construction: slugs are acquired in sorted order,
 * all-or-nothing — any refusal releases everything acquired and the
 * whole set retries until the deadline.
 *
 * Chain membership is re-resolved AFTER the locks are held (records can
 * be rewritten between the unlocked resolve and the acquire — a
 * concurrent destroy's reparent, a `wt base` edit) and must map inside
 * the locked slug set; a chain that grew a member retries against the
 * new shape. The returned chain is the under-lock resolve — callers
 * operate on it directly rather than re-resolving.
 *
 * Known non-participant (audited, accepted): `wt base` / the `b` picker
 * write fork-base records via `setSlugBase` without taking the slug
 * lock, so a base edit landing mid-restack can still race the record
 * writes. Replay's anchor advance is compare-and-set and skips on a
 * moved record; reconcile's reparent is a plain overwrite — worst case
 * a hand edit issued during the seconds a reconcile runs is clobbered
 * by (or clobbers) the reconcile's own reparent, both of which the next
 * reconcile re-derives. The wtstate file itself stays consistent via
 * its own `__wtstate__` flock.
 */
export async function lockChain(
  branch: string,
  phase: string,
): Promise<ChainLockResult> {
  const deadline = Date.now() + 5_000;
  for (;;) {
    const probe = await resolveChain(branch);
    if (!probe) return { status: "gone" };
    const slugs = [...new Set(probe.steps.map((s) => s.slug))].sort();
    const handles: LockHandle[] = [];
    // Everything between the first acquire and the successful return
    // runs under try/catch: a throw (an I/O error in tryAcquireLock, a
    // transient git failure in the verification resolve) with locks
    // already held would otherwise leak them for the life of the
    // process — flock only drops on fd close — wedging those slugs'
    // restack/destroy until a restart.
    try {
      let refused = false;
      for (const slug of slugs) {
        const h = tryAcquireLock(slug, "restack", { phase });
        if (!h) {
          refused = true;
          break;
        }
        handles.push(h);
      }
      if (!refused) {
        const chain = await resolveChain(branch);
        if (!chain) {
          for (const h of handles) h.release();
          return { status: "gone" };
        }
        const locked = new Set(slugs);
        if (chain.steps.every((s) => locked.has(s.slug))) {
          return { status: "ok", chain, handles };
        }
        // Membership grew under us — release and retry against the new shape.
      }
      for (const h of handles) h.release();
    } catch (err) {
      for (const h of handles) h.release();
      throw err;
    }
    if (Date.now() >= deadline) return { status: "busy" };
    // Jitter so two chains repeatedly colliding on a shared member (a
    // stack-on-stack boundary) don't retry in lockstep for the whole
    // deadline (same rationale as the engine's lockBackoff).
    await Bun.sleep(250 + Math.floor(Math.random() * 250));
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
