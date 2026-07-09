/**
 * Barrel re-exporting the split `stack-ops/` modules. See
 * `src/core/stack-ops/shared.ts` for the module overview.
 */
export type { Logger } from "./stack-ops/shared.ts";

export {
  ancestorOwnedHunks,
  isAdoptablePr,
  parseNameStatus,
  validatePartialCoverage,
  validateFileCoverage,
  applyStack,
} from "./stack-ops/apply.ts";
export type { ApplyOptions, ApplyResult } from "./stack-ops/apply.ts";

export { stackStatus } from "./stack-ops/status.ts";
export type { SliceStatusRow, StackStatusReport } from "./stack-ops/status.ts";

export { rebaseStack, replayStack, resolveAnchor } from "./stack-ops/replay.ts";
export type { RebaseOptions, RebaseResult } from "./stack-ops/replay.ts";

export { reconcileStack } from "./stack-ops/reconcile.ts";

export { splitStack } from "./stack-ops/split.ts";
export type { SubSliceSpec, SplitResult } from "./stack-ops/split.ts";

export { addSliceToStack } from "./stack-ops/add.ts";
export type { AddSliceResult } from "./stack-ops/add.ts";

export { pruneStackBackups } from "./stack-ops/prune-backups.ts";
export type { PruneBackupsResult } from "./stack-ops/prune-backups.ts";
