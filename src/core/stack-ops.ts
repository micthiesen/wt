/**
 * Barrel re-exporting the `stack-ops/` restack modules. See
 * `src/core/stack-ops/shared.ts` for the module overview.
 */
export type { Logger } from "./stack-ops/shared.ts";
export { STACK_BUSY } from "./stack-ops/shared.ts";

export { resolveChain } from "./stack-ops/chain.ts";
export type { ChainStep, RestackChain } from "./stack-ops/chain.ts";

export { rebaseStack, replayStack, resolveAnchor } from "./stack-ops/replay.ts";
export type { RebaseOptions, RebaseResult } from "./stack-ops/replay.ts";

export { reconcileStack } from "./stack-ops/reconcile.ts";

export { pruneStackBackups } from "./stack-ops/prune-backups.ts";
export type { PruneBackupsResult } from "./stack-ops/prune-backups.ts";
