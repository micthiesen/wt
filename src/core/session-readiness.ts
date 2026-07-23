import type { LockMeta } from "./types.ts";

/**
 * Interactive sessions only need a materialized checkout. The remaining init
 * phases (env copy, stage pinning, dependency install) can safely continue in
 * parallel, while every other lock still protects the checkout from access.
 */
export function canEnterSessionDuringLock(
  lock: Partial<Pick<LockMeta, "op">> | null | undefined,
  checkoutExists: boolean,
): boolean {
  return !lock || (lock.op === "init" && checkoutExists);
}
