/**
 * Barrel re-exporting the original `core/wtstate.ts` surface. The
 * implementation now lives under `core/wtstate/` in strict layers:
 * `types.ts` (leaf types) → `io.ts` (state-file read/write/lock) →
 * `{sections,removed,automations-pause}.ts` (mutators). See those
 * files for the actual logic and comments.
 */

export {
  GROUP_INBOX,
  STACK_SECTION_PREFIX,
  stackIdFromSectionKey,
  stackSectionKey,
} from "./wtstate/types.ts";
export type {
  RemovedWorktree,
  WtSlugState,
  WtState,
} from "./wtstate/types.ts";

export { WT_STATE_DIR, readWtState } from "./wtstate/io.ts";

export {
  advanceBaseAnchor,
  clearSlugState,
  moveGroupPast,
  placeSlug,
  reapWtState,
  renameSection,
  reparentBaseReferences,
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
