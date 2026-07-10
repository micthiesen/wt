/**
 * Synthetic section key for an inferred stack. NUL-prefixed so it can
 * never collide with a user's manual section name. The TUI re-exports
 * these from `useWorktreeRows.ts`; they live here because
 * `sectionsOrder` — the unified display order over ALL groups (stack
 * sections, the inbox, manual sections) — is owned by this module.
 * The value is persisted (foldedSections, sectionsOrder), so it must
 * never change. The `stackId` half is the stack's root branch, so a
 * stack's persisted rank/fold survives exactly as long as its root
 * does — a re-rooted stack (root landed and cleaned) starts fresh.
 */
export const STACK_SECTION_PREFIX = "\0stack:";
export function stackSectionKey(stackId: string): string {
  return `${STACK_SECTION_PREFIX}${stackId}`;
}
/** Inverse of `stackSectionKey`; `null` for non-stack keys. */
export function stackIdFromSectionKey(key: string): string | null {
  return key.startsWith(STACK_SECTION_PREFIX)
    ? key.slice(STACK_SECTION_PREFIX.length)
    : null;
}

/**
 * Sentinel entry representing the unsectioned inbox in `sectionsOrder`.
 * NUL-prefixed like stack keys so it can't collide with a manual
 * name. Its presence doubles as the migration marker: a state file
 * without it predates unified group ordering and gets seeded with the
 * legacy layout (inbox, then manual sections) on read.
 */
export const GROUP_INBOX = "\0inbox";

export type WtSlugState = {
  /** Section name. `null` = unsectioned (rendered at top, no header). */
  section: string | null;
  /** Manual ordering scalar within (section, archived) bucket. Lower = earlier. */
  order: number;
  /**
   * Branch this worktree is based on, when that isn't trunk — recorded
   * by `wt new --base <ref>`, edited via `wt base` / the TUI `b`
   * picker, and rewritten by a restack reconcile when the parent lands.
   * This record is THE stack primitive: worktrees whose records chain
   * into each other render as a stack, diff against their parent, and
   * replay onto it on restack. May legitimately name trunk after a
   * reconcile reparented the worktree — the branch half is then inert
   * (trunk is the default) but it keeps `baseSha` carrying the anchor.
   */
  baseBranch?: string;
  /**
   * Squash-safe replay anchor recorded alongside `baseBranch`: the
   * parent-tip SHA this worktree's own commits sit on. Captured at
   * creation (`wt new --base`), advanced after every successful replay.
   * The restack engine rebases `--onto <newParentTip> <baseSha>
   * <branch>`, so only the worktree's own commits move — a parent that
   * squash-merged (its commits no longer matching) is excluded by
   * construction. Absent on hand-recorded bases; replay falls back to a
   * merge-base then.
   */
  baseSha?: string;
  /**
   * Per-worktree opt-out from `[[automations]]` (Ctrl+A in the TUI).
   * Present only when true; the engine skips paused slugs entirely
   * (no fires, no queued intents).
   */
  automationsPaused?: boolean;
};

/**
 * History entry for a destroyed worktree — powers the TUI's removed-
 * worktrees view (`h`) and its restore action. Snapshotted at destroy
 * dispatch by the TUI flows (rich: title + PR) and confirmed by
 * `removeWorktree` itself on success (minimal: slug + branch), so CLI
 * removes are tracked too. Merged by slug: defined fields of a newer
 * record win, rich fields survive a later minimal write.
 */
export type RemovedWorktree = {
  slug: string;
  branch: string;
  /** ISO timestamp of the latest destroy dispatch / completion. */
  removedAt: string;
  /** Display title at removal (AI/PR/commit-derived; absent when it was just the slug). */
  title?: string;
  /** PR snapshot at removal, when the branch had one. */
  prNumber?: number;
  prUrl?: string;
  prState?: string;
};

/**
 * Persisted state for the worktree list:
 *  - `slugs`: per-worktree manual section + within-section order, plus
 *    the fork-base record (`baseBranch`/`baseSha`) stacks are inferred
 *    from.
 *  - `sectionsOrder`: the unified display order over every GROUP in the
 *    list — stack section keys (`stackSectionKey(rootBranch)`), the
 *    inbox sentinel (`GROUP_INBOX`), and manual section names, all in
 *    one ranked array. Stack keys enter it lazily (when the user moves
 *    a stack section); unranked stacks sort to the top of the list.
 *
 * Why an explicit array instead of deriving section position from
 * `min(order)` of members: derived ordering causes a section to leap
 * up or down whenever its first item moves out, which the user noticed
 * as "weird unexpected reordering". Manual sections still feel ephemeral
 * (auto-appended on first encounter, pruned when no slug references
 * them) — this array is just a sort hint, not user-managed metadata.
 */
export type WtState = {
  slugs: Record<string, WtSlugState>;
  sectionsOrder: string[];
  /** Section keys the user has folded in the list (persisted across restarts). */
  foldedSections: string[];
  /**
   * Stack ids (root branches) whose automations are paused (Ctrl+A on
   * any stack member or its folded header). Keyed by the stack id
   * rather than member slugs so members added later are covered too.
   * An id whose stack is gone is inert and eventually rotates out on
   * toggle writes.
   */
  pausedStacks: string[];
  /** Global automations pause (Shift+A). Persisted across restarts. */
  automationsPaused: boolean;
  /**
   * Recently destroyed worktrees, newest first. Capped + age-pruned at
   * write time (`recordRemovedWorktrees`); an entry whose slug is live
   * again is display-filtered by the TUI and cleared by `createWorktree`.
   */
  removed: RemovedWorktree[];
};
