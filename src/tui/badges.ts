/**
 * Shared badge mapping helpers — the single source of truth for any
 * concept rendered as a glyph in both the row list and the details
 * pane. Adding a new badge? Add it here, not inline in a panel.
 *
 * UX rules this module enforces:
 *
 * 1. **Same concept = same glyph everywhere.** Anything shown as an
 *    icon in the list pane uses the SAME icon in the details pane.
 *    The list teaches itself by reading the details once. Never
 *    inline a different glyph for the same concept in another file.
 *
 * 2. **Glyph + adjacent text share color.** Render `<icon> <text>`
 *    as one badge — wrap them in a single fg span using the badge's
 *    color. Text that's part of the badge's label (e.g. `#4546`
 *    after a PR icon, "checks" after a check icon) takes the badge
 *    color. Standalone metadata (separators, parentheticals, dim
 *    hints) uses theme.fgDim.
 *
 * 3. **In the details pane, every state of a state-machine row
 *    gets an icon.** Stage (deployed / not-deployed), status (clean
 *    / dirty / merged / gone / missing / busy), pr (open / draft /
 *    merged / closed) — pick an icon for *every* state, not just
 *    the "active" ones. Color carries the active/inactive
 *    distinction: saturated palette for active states, theme.fgDim
 *    for default/quiet states. Don't pick tiny dot/circle
 *    codepoints to fill the "quiet" slot — they render undersized
 *    next to shape-based siblings (pencil, merge, leaf, etc.) and
 *    the row reads visually unbalanced. Prefer real shapes.
 *
 *    The list pane is intentionally different: it uses
 *    absence-as-signal for row badges (bolt, merge-queue, prState,
 *    checks) so density stays high — most rows show a sparse
 *    cluster, blocked/active rows pop. The badge GLYPHS are
 *    shared with the details pane (rule #1), but the list omits
 *    the badge entirely for the "quiet" state rather than
 *    rendering an off-variant.
 *
 *    The "no value at all" case (no PR exists, no linear ID) is
 *    different from a quiet state — render the whole row's value
 *    as `—`, not a state badge.
 *
 * 4. **Two spaces between PUA glyph and text.** opentui's native
 *    renderer treats PUA codepoints as 1-cell wide even though our
 *    font renders them 2-cell. The extra space prevents the icon's
 *    right half from overlapping the next char.
 */
import { config, type IssueStatusStyle } from "../core/config.ts";
import type { MergeConflictProbe } from "../core/git.ts";
import type { DerivedState } from "../core/harness/status.ts";
import {
  type LockMeta,
  type MergeQueueEntry,
  type MergeQueueState,
  type PrChecks,
  type PrReview,
  type PullRequest,
  type ReviewBotStatus,
  type Status,
  StatusKind,
} from "../core/types.ts";
import {
  effectiveWorkState,
  verificationOverdue,
  type WorkState,
  type WorkStatusRecord,
} from "../core/work-status.ts";

import { NF } from "./icons.ts";
import { theme } from "./theme.ts";

export type Badge = { glyph: string; fg: string };

/** Tracker vocabulary and colors belong to config, never a provider in wt. */
export function issueStatusBadge(status: string | undefined, styles: Readonly<Record<string, IssueStatusStyle>> = config.issueTracker?.statusStyles ?? {}): Badge {
  const style = status && Object.hasOwn(styles, status) ? styles[status] : undefined;
  const icons: Record<IssueStatusStyle["icon"], string> = {
    circle: NF.dotOutline, backlog: NF.checkPend, progress: NF.dotCircle,
    review: NF.halfCircle, completed: NF.taskComplete, cancelled: NF.taskCancelled, blocked: NF.slash,
  };
  return style ? { glyph: icons[style.icon], fg: style.color } : { glyph: NF.dotOutline, fg: theme.fgDim };
}

/** Glyph + color for a worktree's status — used by row marker AND git-line verb. */
export function statusBadge(s: Status): Badge {
  if (s.kind === StatusKind.Busy) {
    if (s.op === "remove") return { glyph: NF.trash, fg: theme.err };
    if (s.op === "restack") return { glyph: NF.restack, fg: theme.accent };
    return { glyph: NF.rocket, fg: theme.accent };
  }
  if (s.kind === StatusKind.Missing) return { glyph: NF.unlink, fg: theme.err };
  if (s.kind === StatusKind.Gone) return { glyph: NF.slash, fg: theme.warn };
  if (s.kind === StatusKind.Merged) return { glyph: NF.merge, fg: theme.ok };
  if (s.kind === StatusKind.Dirty) return { glyph: NF.pencil, fg: theme.warn };
  return { glyph: NF.clean, fg: theme.fgDim };
}

/**
 * Color for a work status (the agent-asserted lifecycle state). One
 * hue per state, chosen so a scan down the dot column answers "what
 * needs me": red = blocked on the human, yellow = verification
 * pending, green = merge it, magenta = in review, cyan = in flight,
 * dim = queued or finished. Red is reused for an overdue post-merge
 * verification (`workStatusBadge`), which is the same message —
 * something is waiting on a person — arriving by another route.
 */
export function workStateColor(state: WorkState): string {
  switch (state) {
    case "needs-human":
      return theme.err;
    case "needs-testing":
      return theme.warn;
    case "ready":
      return theme.ok;
    case "review":
      return theme.info;
    case "working":
      return theme.accent;
    case "todo":
    case "verified":
    case "dropped":
      return theme.fgDim;
  }
}

/** The dot glyph for a work state: solid, hollow for `todo`, a dim
 *  circle-slash for `dropped` (will never land — visually kin to the
 *  gone marker, but in the status-dot slot and always dim). */
export function workStateGlyph(state: WorkState): string {
  if (state === "dropped") return NF.slash;
  // Landed and confirmed: the merge glyph in the dot slot, dim. It is
  // the one terminal state that succeeded, and reusing the shape the
  // row already wears for "merged" says so without minting a glyph.
  if (state === "verified") return NF.merge;
  return state === "todo" ? NF.dotOutline : NF.dot;
}

/**
 * The work-status dot — the list pane's leftmost glyph (the slot the
 * old clean/dirty marker held; dirty moved into the badge cluster,
 * clean renders nothing). Solid circle for every asserted state,
 * hollow for `todo`; color is the whole signal. Derived upgrades from
 * `effectiveWorkState` apply (a session asking for input renders the
 * needs-human red even without an assertion). A row with no assertion
 * renders the same hollow dim dot as `todo` — "not started" is the
 * honest default, and a blank slot reads as a rendering gap rather
 * than a state. Render-time only: nothing is ever written back.
 */
export function workStatusBadge(
  record: WorkStatusRecord | null | undefined,
  sessionState?: DerivedState,
  stale = false,
  landed = false,
): Badge {
  const eff = effectiveWorkState(record, sessionState, landed);
  if (!eff) return { glyph: NF.dotOutline, fg: theme.fgDim };
  // A post-merge verification that has aged out goes RED, not warn.
  // The whole hazard this field addresses is a row that reads as
  // covered while nothing happens, and warn-yellow is what every other
  // pending-verification row already wears — indistinguishable at a
  // glance from one asserted twenty minutes ago. Placed above the gate
  // branch: an overdue verification is louder than a gate, and the two
  // cannot co-occur anyway (a gate says do not merge, this one needs
  // the merge to have happened).
  if (verificationOverdue(record, landed)) {
    return { glyph: NF.dot, fg: theme.err };
  }
  // A gated `ready` must not wear ready's green dot. The dot is what a
  // scan of the board reads, and green sitting in the merge band is the
  // whole reason a branch gated on a mobile release got queued for
  // merge twice.
  //
  // Circle-slash in warn, reusing `dropped`'s glyph in a different
  // color rather than minting one: the shape says "no" at a glance and
  // is already proven to render, and color is this slot's signal by
  // convention. Not dim-and-hollow, which would collide with `todo` and
  // with statusless — a finished branch must not look unstarted.
  if (eff.blocked) return { glyph: NF.slash, fg: theme.warn };
  return {
    // Stale (commits landed after the assertion — see
    // `isWorkStatusStale`) hollows the dot but keeps the state color:
    // green-but-hollow reads "was ready, tree moved since", distinct
    // from both a solid trustworthy dot and todo's dim hollow. A
    // session-derived state (`asking`) is live information, never
    // hollowed — staleness describes the RECORD, not the session.
    glyph: stale && !eff.derived ? NF.dotOutline : workStateGlyph(eff.state),
    fg: workStateColor(eff.state),
  };
}

/**
 * Is "merge when ready" armed, given the two places the answer hides?
 *
 * Only one of the two GitHub features behind that one button sets
 * `autoMerge`: on a base branch with a merge queue, arming ENQUEUES and
 * `autoMergeRequest` stays null forever, so the queue entry is the only
 * evidence there will ever be. `mergeWhenReadyArmed` in app-helpers is
 * this same question asked of a row.
 */
export function armedFromPr(
  pr: PullRequest,
  mq: MergeQueueEntry | null | undefined,
): boolean {
  return pr.autoMerge != null || mq != null;
}

/** Glyph + color for a PR's state — used by row badge cluster AND details pr line. */
export function prStateBadge(pr: PullRequest): Badge {
  if (pr.state === "MERGED") return { glyph: NF.prMerged, fg: theme.info };
  if (pr.state === "CLOSED") return { glyph: NF.prClosed, fg: theme.err };
  if (pr.isDraft) return { glyph: NF.prDraft, fg: theme.fgDim };
  return { glyph: NF.prOpen, fg: theme.accentAlt };
}

/**
 * What the LIST's PR slot shows. That slot already swaps between the PR
 * glyph and the merge-queue indicator (icon + position, 4 cells);
 * armed-but-not-queued is the third case and renders the same icon
 * without a position, in the same colour the details pane's own
 * auto-merge segment uses.
 *
 * It lives here rather than inside `prStateBadge` because armed is not
 * a PR STATE — the PR is open either way — and because the details
 * pane must NOT take the swap: it has room for a dedicated auto-merge
 * segment, and swapping there would print the same icon twice on one
 * line. The list has no such room, which is the whole reason the slot
 * swaps at all.
 *
 * Only reachable with no queue entry in practice (callers render the
 * position when there is one), but it takes `mq` and checks it anyway,
 * so a caller that forgets the branch degrades to the position rather
 * than silently dropping it.
 */
export function prSlotBadge(pr: PullRequest, mq?: MergeQueueEntry | null): Badge {
  if (!mq && pr.state === "OPEN" && !pr.isDraft && armedFromPr(pr, mq)) {
    return { glyph: NF.mergeQueue, fg: theme.info };
  }
  return prStateBadge(pr);
}

/**
 * Glyph + color for a PR's CI rollup — used by the list cluster AND the
 * details checks segment. Null for the quiet `none` state so both panes
 * omit it (absence-as-signal, rule #3).
 */
export function checkBadge(c: PrChecks): Badge | null {
  switch (c) {
    case "pass":
      return { glyph: NF.checkPass, fg: theme.ok };
    case "fail":
      return { glyph: NF.checkFail, fg: theme.err };
    case "pending":
      return { glyph: NF.checkPend, fg: theme.warn };
    default:
      return null;
  }
}

/**
 * Glyph + color for human review state. Approved gets a thumbs-up;
 * changes-requested gets a lightbulb (suggestions). `pending` and
 * `unrequested` share the eye glyph and are told apart by color (warn =
 * asked + waiting, dim = nobody asked yet) — the eye rather than a clock
 * so review-pending doesn't collide with the CI pending clock
 * (`checkPend`). Null for the quiet `none` state.
 *
 * `changes_requested` is intentionally amber (`theme.warn`), not alarm-red
 * (`theme.err`): "needs another pass" reads softer than "rejected." It
 * shares amber with `pending` but stays distinct via the lightbulb glyph.
 */
export function reviewBadge(r: PrReview): Badge | null {
  if (!config.github.reviewers) return null;
  switch (r) {
    case "approved":
      return { glyph: NF.thumbsUp, fg: theme.ok };
    case "changes_requested":
      return { glyph: NF.lightbulb, fg: theme.warn };
    case "pending":
      return { glyph: NF.eye, fg: theme.warn };
    case "unrequested":
      return { glyph: NF.eye, fg: theme.fgDim };
    default:
      return null;
  }
}

/**
 * Session states that count as "the AI is actively engaged here" for
 * the rebase slot: a conflicted row whose session is in one of these
 * renders as being-resolved (warn sync) instead of raw conflict (red).
 * `asking` counts — a `/restack` pausing on a question is mid-task.
 * `unknown` does not: a live session we can't read stays honest-red.
 */
const ENGAGED_SESSION_STATES: ReadonlySet<DerivedState> = new Set([
  "working",
  "polling",
  "asking",
]);

/**
 * Glyph + color for the rebase-lifecycle slot the list cluster and the
 * details-pane rebase block share. One slot, four states, priority order:
 *
 *  - **restacking** (accent sync glyph): the engine holds this
 *    worktree's per-slug lock (`op: "restack"`) — reconcile/replay in
 *    flight across the whole chain.
 *  - **rebasing** (warn sync glyph): the worktree sits mid-rebase — a
 *    `/restack` or hand rebase resolving a conflict. (The engine itself
 *    always aborts before bailing, so right after a bail the row shows
 *    resolving or conflict; this state appears once the resolving
 *    rebase starts.) Same glyph as restacking; color carries the stage.
 *  - **resolving** (warn sync glyph): the branch still conflicts with
 *    its base AND the row's active session is engaged (working /
 *    polling / asking) — the conflict-bail handoff's cold start,
 *    context reading, and `wt restack` re-runs all land here, so the
 *    slot doesn't flash red mid-handoff. Level-derived from live
 *    session state rather than "did wt dispatch a handoff", so a
 *    hand-typed `/restack` reads identically; when the session goes
 *    idle without fixing the conflict, red returns — the honest state.
 *  - **conflict** (err alert triangle): the pre-flight `merge-tree`
 *    dry-run says HEAD won't land cleanly on its base and nothing is
 *    working on it.
 *
 * Null for clean/unknown so both panes keep absence-as-signal. The
 * engine's own transient rebases never flash warn: the lock is held for
 * the duration, and restacking outranks rebasing.
 */
export function rebaseBadge(
  lock: Partial<LockMeta> | null | undefined,
  probe: MergeConflictProbe | undefined,
  sessionState?: DerivedState,
): Badge | null {
  if (lock?.op === "restack") return { glyph: NF.restack, fg: theme.accent };
  if (probe?.status === "rebasing") return { glyph: NF.restack, fg: theme.warn };
  if (probe?.status === "conflict") {
    if (sessionState && ENGAGED_SESSION_STATES.has(sessionState)) {
      return { glyph: NF.restack, fg: theme.warn };
    }
    return { glyph: NF.conflict, fg: theme.err };
  }
  return null;
}

/**
 * The review-bot glyph: CodeRabbit keeps its whimsy carrot; any other
 * configured `[review_bot]` gets the checklist (deliberately NOT a
 * robot — the claude harness glyph is nf-md-robot, and an identical
 * glyph one slot over reads as a phantom second session).
 */
export const REVIEW_BOT_GLYPH =
  config.reviewBot.login === "coderabbitai" ? NF.carrot : NF.reviewChecklist;

/**
 * Glyph + color for review-bot state. Single glyph, color-coded: it
 * echoes the human-review palette one notch softer — pending (warn),
 * clean (ok). Unresolved findings are "address these", not a rejection,
 * so info (the magenta "look-here" tier) rather than changes-requested
 * red. Color is load-bearing here — the glyph has no clean
 * state-specific variants. Null for the quiet `none` state.
 *
 * A stale clean review stays green. Staleness rides in the details-pane
 * prose (`reviewed (old head)`) instead: checklist bots that only review
 * on `opened` make stale the STEADY state, not a transient, so dimming
 * it made "the bot found nothing" a colour you'd essentially never see —
 * the badge stopped answering the question it exists to answer.
 */
export function reviewBotBadge(rb: ReviewBotStatus): Badge | null {
  switch (rb.state) {
    case "unresolved":
      return { glyph: REVIEW_BOT_GLYPH, fg: theme.info };
    case "pending":
      return { glyph: REVIEW_BOT_GLYPH, fg: theme.warn };
    case "clean":
      // A review of an OLDER commit is not a clean bill of health for
      // this one, and green is the one color that says "nothing to do
      // here" without anyone opening the details pane. The `(old head)`
      // prose was the only tell, and prose beside a contradicting glyph
      // does not exist. Unknown fails toward the warning: a false yellow
      // costs a look, a false green costs the review.
      return { glyph: REVIEW_BOT_GLYPH, fg: rb.stale ? theme.warn : theme.ok };
    default:
      return null;
  }
}

/**
 * Whether the bot badge renders at all for this PR. Shared by the
 * list-pane cluster and the details-pane segment so the two can't drift.
 *
 * The draft gate is mode-specific, because "the bot skipped this draft"
 * only mimics `clean` in one of the two modes:
 *
 * - `threads` (CodeRabbit): clean is inferred from the bot's check
 *   context completing, and its "review skipped — draft detected" run
 *   completes exactly like a real one. Hide, or a skipped draft reads
 *   green.
 * - `checklist`: clean requires a summary comment the bot actually
 *   posted, so a skip yields `none` and the badge is already absent.
 *   Hiding here only suppresses REAL reviews — these bots review on
 *   `opened` whether or not the PR is a draft.
 */
const BOT_SHOWS_ON_DRAFT = config.reviewBot.unresolvedVia === "checklist";

export function showReviewBot(pr: {
  state: PullRequest["state"];
  isDraft: boolean;
}): boolean {
  if (pr.state !== "OPEN") return false;
  return !pr.isDraft || BOT_SHOWS_ON_DRAFT;
}

/**
 * Merge-queue state → color + label — used by the list cluster's bare
 * indicator (color only) AND the details-pane segment (color + text).
 * Severity tiers: green = about to land, yellow = waiting on checks or
 * behind others, red = blocked/failed. Unknown states pass their raw
 * value through as `text` (dim) so a new GitHub enum surfaces rather
 * than vanishing.
 */
export function mqStateBadge(state: MergeQueueState): { text: string; fg: string } {
  switch (state) {
    case "MERGEABLE":
      return { text: "mergeable", fg: theme.ok };
    case "AWAITING_CHECKS":
      return { text: "awaiting checks", fg: theme.warn };
    case "QUEUED":
      return { text: "queued", fg: theme.warn };
    case "UNMERGEABLE":
      return { text: "unmergeable", fg: theme.err };
    case "LOCKED":
      return { text: "locked", fg: theme.err };
    default:
      return { text: state, fg: theme.fgDim };
  }
}
