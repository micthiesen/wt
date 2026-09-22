/**
 * Barrel re-exporting the original `core/wtstate.ts` surface. The
 * implementation now lives under `core/wtstate/` in strict layers:
 * `types.ts` (leaf types) → `io.ts` (SQLite read/write/lock) →
 * `{sections,removed,automations-pause}.ts` (mutators). See those
 * files for the actual logic and comments.
 */

export {
  GROUP_ARCHIVED,
  GROUP_INBOX,
  STACK_SECTION_PREFIX,
  stackIdFromSectionKey,
  stackSectionKey,
} from "./wtstate/types.ts";
export type {
  RemovedWorktree,
  ReviewRequestDismissal,
  WorktreeLayout,
  WtSlugState,
  WtState,
} from "./wtstate/types.ts";

export { parseWtState, WT_STATE_DIR, readWtState } from "./wtstate/io.ts";

export {
  advanceBaseAnchor,
  claimDevPort,
  clearSlugState,
  moveGroupPast,
  placeSlug,
  reapRemoteLayouts,
  removeSection,
  reapWtState,
  renameSection,
  reparentBaseReferences,
  setSlugBase,
  recordSlugCreated,
  setSlugDevPort,
  setSlugDevStartedSha,
  setSlugExamined,
  setSlugGithubIssue,
  setSlugIssueId,
  setSlugSection,
  setWorktreeSection,
  setSlugWorkStatus,
  setSectionFolded,
  swapOrders,
  toggleSectionFolded,
  setBranchTip,
} from "./wtstate/sections.ts";

export {
  clearRemovedWorktree,
  isMergedRemoval,
  recentlyRemovedWorktrees,
  recentRemovalsSummary,
  removedJsonEntry,
  verificationOwedAtRemoval,
  recordRemovedWorktrees,
} from "./wtstate/removed.ts";

export {
  toggleGlobalAutomationsPaused,
  toggleRemovedAutomationsPaused,
  toggleSlugAutomationsPaused,
  toggleStackAutomationsPaused,
} from "./wtstate/automations-pause.ts";

export { setAttentionSeen } from "./wtstate/attention.ts";

export {
  addReviewRequestDismissal,
  dismissReviewRequest,
} from "./wtstate/review-requests.ts";

export {
  pruneMergeEdges,
  removeMergeEdge,
  setMergeEdge,
} from "./wtstate/edges.ts";
