import type { MergeEdge } from "../merge-edges.ts";
import type { WorkStatusRecord } from "../work-status.ts";

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

/**
 * Sentinel for the archived block's fold state. NUL-prefixed for the
 * same reason as the inbox, and deliberately NOT a member of
 * `sectionsOrder`: the archived block is pinned to the bottom of the
 * list and holds no manual ordering. It appears in `foldedSections`
 * and nowhere else, which is why nothing had to change to persist it.
 */
export const GROUP_ARCHIVED = "\0archived";

export type WorktreeLayout = {
  /** Section name. `null` = the controller's Inbox. */
  section: string | null;
  /** Manual ordering scalar within the section. Lower = earlier. */
  order: number;
};

export type WtSlugState = WorktreeLayout & {
  /** Successful wt creation, written once for this checkout; absent on legacy rows. */
  createdAt?: string;
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
   * Port allocated to this worktree's `[dev_server]` process. Assigned
   * on first start from the configured range and kept stable so
   * browser tabs survive restarts; freed when the slug's state is
   * cleared (worktree destroy / re-create).
   */
  devPort?: number;
  /**
   * HEAD when this worktree's dev server was last started.
   *
   * The point is not "how old is it" but whether the commits it came
   * up on are still ANCESTORS of HEAD. Ordinary commits keep it an
   * ancestor and a hot-reloading server handles them; a rebase, reset
   * or restack does not, and that is precisely when a dev environment
   * caching anything derived from the tree (a migrated database above
   * all) goes silently wrong — the schema stays where it was while the
   * files move underneath it.
   *
   * A recorded sha and an ancestor test cost one `git merge-base
   * --is-ancestor` (0.1s here). Asking the environment itself is the
   * precise answer and is 90x more expensive: a `docker exec psql`
   * against a live Supabase stack measured 9s on this machine, which
   * is why that check is on-demand (`[dev_server] health_command`) and
   * this one can ride a poll.
   */
  devStartedSha?: string;
  /**
   * Per-worktree opt-out from `[[automations]]` (Ctrl+A in the TUI).
   * Present only when true; the engine skips paused slugs entirely
   * (no fires, no queued intents).
   */
  automationsPaused?: boolean;
  /**
   * GitHub issue number attached to this worktree as its SECONDARY id
   * — the primary id stays the tracker id parsed from the slug
   * (`eng-1935-…` → ENG-1935). Set by `wt new --gh <n>` or `wt issue
   * <slug> --gh <n>`, typically after a spec/breakout issue is created
   * mid-work (which is why it's state, not part of the branch name:
   * renaming a pushed branch to encode it would be disruptive). The
   * TUI's `i`/`y i` treat it as the most-specific link target.
   */
  githubIssue?: number;
  /**
   * Tracker id for this worktree when the SLUG does not carry one
   * (`camera-selection-sticky` with nothing to parse) or carries the
   * wrong one. Set by `wt issue <slug> --id COZ-2185` or the TUI's
   * `I` picker; cleared with `--clear-id`.
   *
   * An OVERRIDE, never a cache: every reader goes through
   * `resolveIssueId`, which prefers this and falls back to parsing the
   * slug. Deliberately not backfilled from slugs by a migration —
   * a slug-derivable id is free to re-derive and self-expires when the
   * branch is renamed, so storing it would encode a fact needing
   * upkeep. What is stored here is exactly the part that CANNOT be
   * derived, which is why it is safe to store at all.
   *
   * Stored uppercased and validated against `ISSUE_ID_RE` at the
   * boundary, so `{{issue_id}}` renders the same shape whichever half
   * of `resolveIssueId` answered.
   */
  issueId?: string;
  /**
   * "Someone with fleet context looked at this row and concluded X."
   *
   * The half of coordination nothing recorded. A shepherd sweeping the
   * fleet every few minutes spends its attention re-deriving verdicts
   * it already reached: one ran the same two-call review query against
   * the same two pull requests on four consecutive passes and got the
   * same empty answer every time, because the rows LOOKED interesting
   * (a review job reports failed while its review is still running) and
   * nothing recorded that the question had already been asked and
   * answered.
   *
   * Write-once and self-expiring, which is the only reason it is safe
   * to store: the verdict is stamped with the sha it was reached at, so
   * it evaporates the moment the branch moves — and a branch moving is
   * exactly when a verdict stops being trustworthy. It is a SKIP HINT,
   * never authority: absent, stale or unrecognised all mean "look
   * properly", so the failure direction is wasted work rather than a
   * missed row.
   */
  examined?: {
    /** HEAD when the conclusion was reached. Any other HEAD voids it. */
    sha: string;
    /**
     * The row's BASE head at the same moment, and the half that keeps
     * this honest.
     *
     * Keying on the row alone was wrong in a way that inverted the
     * field's whole safety property. A pull request goes from behind to
     * CONFLICTING because the BASE moved, not because the row did — so
     * a row-only key stays valid across exactly the event that makes a
     * verdict worthless, and a sweep skips the one row that most needs
     * looking at. Caught before it bit: a coordinator was about to
     * stamp "behind only, no action" across 28 pull requests at the
     * start of a merge batch.
     *
     * Invalidating on base movement costs re-examination during a merge
     * batch and nothing at all when the trunk is quiet — which is the
     * right way round twice over, because a settled fleet is when the
     * saving matters and a moving trunk is when every mergeability
     * verdict is suspect anyway.
     *
     * Absent on records written before this existed: those cannot prove
     * the base held still, so they read as void rather than current.
     */
    baseSha?: string;
    /** What was concluded, in the examiner's words. */
    verdict: string;
    /** `WT_AGENT` of whoever looked (`manager`, a slug), when stamped. */
    by?: string;
    at: string;
  };
  /**
   * Agent-asserted lifecycle status (`wt status` / the `u` picker).
   * See `core/work-status.ts` for the vocabulary and semantics.
   * Absent = never asserted (renders as no dot, sorts neutral).
   */
  work?: WorkStatusRecord;
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
  /**
   * The work status the row held when its checkout went away, copied
   * from `slugs[slug].work` by `recordRemovedWorktrees`.
   *
   * The per-slug record is reaped along with the worktree, so without
   * this a merged-and-swept row and a row swept while still owing a
   * post-merge verification are the same empty answer — and that
   * silence cost a fleet manager an issue filed to preserve a check
   * that had in fact already been run and recorded. Absent means
   * UNKNOWN (the row predates this field, or held no status), never
   * "nothing was owed".
   */
  work?: WorkStatusRecord;
  /**
   * The per-worktree automations pause (Ctrl+A), copied off
   * `slugs[slug].automationsPaused` by `recordRemovedWorktrees` and
   * toggleable on the archived row itself.
   *
   * It has to outlive the checkout because the automations that fire
   * for a slug with no checkout are exactly the ones a pause is wanted
   * for: a post-merge `external` run survives its row's death by
   * design (`isPostMergeExternalRun`), so before this the pause a human
   * set was reaped by the sweep seconds after the merge and the run it
   * was set to stop fired anyway.
   *
   * Unlike `work`, absence here is not "unknown" — it means not paused,
   * exactly as an absent flag on a live row does.
   */
  automationsPaused?: boolean;
};

/**
 * A review-request row the user dismissed with `d` in the TUI.
 *
 * The PR's `updatedAt` makes the dismissal self-expiring: wt hides only the
 * exact request snapshot the user handled. Any later PR activity changes the
 * fingerprint and lets the row surface again instead of turning this into a
 * permanent, silently stale ignore list.
 */
export type ReviewRequestDismissal = {
  url: string;
  updatedAt: string;
  dismissedAt: string;
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
  /**
   * Schema version, stamped by every write path (`writeWtState`) and by
   * the migration system on read (`migrateRawWtState` in
   * migrations.ts). Not hand-edited; a shape change bumps
   * `WT_STATE_VERSION` and adds a migration rather than this field
   * moving on its own.
   */
  version: number;
  slugs: Record<string, WtSlugState>;
  /**
   * Controller-owned placement for worktrees executed on another host.
   * Keys are location-aware ledger keys (`@remote/<host>/<slug>`), so a
   * remote slug never collides with a local operational slug record.
   */
  remoteLayouts: Record<string, WorktreeLayout>;
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
   * Attention-feed "seen" watermark (`x` while the attention feed is
   * displayed): events at or before this epoch-ms render dim below a
   * `── seen` rule, so the feed reads "only new stuff" at a glance.
   * Persisted here (not in-memory) so the boot backfill re-seeds
   * already-handled events dimmed instead of resurrecting them bright.
   * `0` = never marked. Display-only: the firehose and the daily log
   * are untouched.
   */
  attentionSeenTs: number;
  /**
   * Recently destroyed worktrees, newest first. Capped + age-pruned at
   * write time (`recordRemovedWorktrees`); an entry whose slug is live
   * again is display-filtered by the TUI and cleared by `createWorktree`.
   */
  removed: RemovedWorktree[];
  /** Recent, snapshot-keyed review requests dismissed from the pinned list. */
  reviewRequestDismissals: ReviewRequestDismissal[];
  /**
   * Last tip wt observed for each branch a `branch.advanced` automation
   * watches. Advanced only when a fire is DISPATCHED, never on mere
   * observation: the range between two tips is consumed exactly once,
   * and moving the mark before the run is delivered would drop it.
   *
   * An absent entry means the branch has not been seen yet, and the
   * first sight records it WITHOUT firing — reading absence as "the
   * beginning of history" would fire once for every commit the branch
   * has ever carried.
   */
  branchTips: Record<string, string>;
  /**
   * Merge edges — pairwise, self-expiring ordering assertions between
   * worktrees (`wt edge`). Vocabulary and design rules live in
   * `core/merge-edges.ts`; writers in `wtstate/edges.ts`. Edges with a
   * dead endpoint are pruned at reap time; stale edges (an endpoint
   * moved past its recorded anchor) are a RENDER-time derivation,
   * never written back.
   */
  edges: MergeEdge[];
};
