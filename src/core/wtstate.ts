/**
 * Barrel re-exporting the original `core/wtstate.ts` surface. The
 * implementation now lives under `core/wtstate/` in strict layers:
 * `types.ts` (leaf types + lenient parse helpers) → `validate.ts` (strict
 * ingest validation) → `io.ts` (state-file read/write/lock) →
 * `{sections,removed,automations-pause,stacks}.ts` (mutators). See those
 * files for the actual logic and comments.
 */

export {
  GROUP_INBOX,
  STACK_SECTION_PREFIX,
  coerceHunkContext,
  coercePartials,
  isUnsafeSlicePath,
  stackIdFromSectionKey,
  stackSectionKey,
} from "./wtstate/types.ts";
export type {
  PartialFile,
  RemovedWorktree,
  StackLimits,
  StackManifest,
  StackSlice,
  StackSliceStatus,
  WtSlugState,
  WtState,
} from "./wtstate/types.ts";

export { validateStackManifest } from "./wtstate/validate.ts";
export type { ManifestValidation } from "./wtstate/validate.ts";

export { WT_STATE_DIR, readWtState } from "./wtstate/io.ts";

export {
  clearBaseReferences,
  clearSlugState,
  moveGroupPast,
  placeSlug,
  reapWtState,
  renameSection,
  setSlugBase,
  setSlugSection,
  swapOrders,
  toggleSectionFolded,
} from "./wtstate/sections.ts";

export { clearRemovedWorktree, recordRemovedWorktrees } from "./wtstate/removed.ts";

export {
  toggleGlobalAutomationsPaused,
  toggleSlugAutomationsPaused,
  toggleStackAutomationsPaused,
} from "./wtstate/automations-pause.ts";

export {
  findStackIdByBranch,
  getStackManifest,
  listStackManifests,
  patchStackManifest,
  putStackManifest,
  updateStackSlice,
} from "./wtstate/stacks.ts";
