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
 *    absence-as-signal for row badges (bolt, mergeability, prState,
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
import {
  type PrChecks,
  type PrReview,
  type PullRequest,
  type RabbitStatus,
  type Status,
  StatusKind,
} from "../core/types.ts";

import { NF } from "./icons.ts";
import { theme } from "./theme.ts";

export type Badge = { glyph: string; fg: string };

/** Glyph + color for a worktree's status — used by row marker AND git-line verb. */
export function statusBadge(s: Status): Badge {
  if (s.kind === StatusKind.Busy) {
    if (s.op === "remove") return { glyph: NF.trash, fg: theme.err };
    return { glyph: NF.rocket, fg: theme.accent };
  }
  if (s.kind === StatusKind.Missing) return { glyph: NF.unlink, fg: theme.err };
  if (s.kind === StatusKind.Gone) return { glyph: NF.slash, fg: theme.warn };
  if (s.kind === StatusKind.Merged) return { glyph: NF.merge, fg: theme.ok };
  if (s.kind === StatusKind.Dirty) return { glyph: NF.pencil, fg: theme.warn };
  return { glyph: NF.clean, fg: theme.fgDim };
}

/** Glyph + color for a PR's state — used by row badge cluster AND details pr line. */
export function prStateBadge(pr: PullRequest): Badge {
  if (pr.state === "MERGED") return { glyph: NF.prMerged, fg: theme.info };
  if (pr.state === "CLOSED") return { glyph: NF.prClosed, fg: theme.err };
  if (pr.isDraft) return { glyph: NF.prDraft, fg: theme.fgDim };
  return { glyph: NF.prOpen, fg: theme.accentAlt };
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
 * Glyph + color for human review state. Approved / changes-requested get
 * distinct shapes (thumbs up/down); `pending` and `unrequested` share
 * the eye glyph and are told apart by color (warn = asked + waiting, dim
 * = nobody asked yet) — the eye rather than a clock so review-pending
 * doesn't collide with the CI pending clock (`checkPend`). Null for the
 * quiet `none` state.
 */
export function reviewBadge(r: PrReview): Badge | null {
  switch (r) {
    case "approved":
      return { glyph: NF.thumbsUp, fg: theme.ok };
    case "changes_requested":
      return { glyph: NF.thumbsDown, fg: theme.err };
    case "pending":
      return { glyph: NF.eye, fg: theme.warn };
    case "unrequested":
      return { glyph: NF.eye, fg: theme.fgDim };
    default:
      return null;
  }
}

/**
 * Glyph + color for CodeRabbit state. Single carrot glyph, color-coded:
 * it echoes the human-review palette one notch softer — pending↔grazing
 * (warn), clean↔resting (ok). Unresolved threads are "address these",
 * not a rejection, so info (the magenta "look-here" tier) rather than
 * changes-requested red. Color is load-bearing here — the carrot family
 * has no clean state-specific variants. Null for the quiet `none` state.
 */
export function rabbitBadge(rb: RabbitStatus): Badge | null {
  switch (rb.state) {
    case "unresolved":
      return { glyph: NF.carrot, fg: theme.info };
    case "pending":
      return { glyph: NF.carrot, fg: theme.warn };
    case "clean":
      return { glyph: NF.carrot, fg: theme.ok };
    default:
      return null;
  }
}
