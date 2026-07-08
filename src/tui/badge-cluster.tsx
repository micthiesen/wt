/**
 * The right-aligned badge cluster a worktree row shows in the list pane:
 * action-running glyph, deploy bolt, harness session glyph, CodeRabbit /
 * review hints, PR-state (or merge-queue) slot, CI rollup. Extracted from
 * the list panel so the folded section/stack summaries in the details
 * pane render the exact same cluster per member — same glyphs, same
 * colors, same order. Individual glyph/color rules still come from
 * `badges.ts`; this module owns the composition.
 */
import {
  type Badge,
  checkBadge,
  prStateBadge,
  rabbitBadge,
  reviewBadge,
} from "./badges.ts";
import { NF } from "./icons.ts";
import { theme } from "./theme.ts";
import { getHarness } from "../core/harness/index.ts";
import type { HarnessId } from "../core/harness/index.ts";
import type { DerivedState } from "../core/claude-status.ts";
import { stateColor } from "./claude-state.ts";
import type { MergeQueueState } from "../core/types.ts";
import type { WorktreeRow } from "./hooks/useWorktreeRows.ts";

/**
 * Cells the badge cluster occupies for a given row. Mirrors the
 * width-prop layout in the JSX below: 2-cell leading gap + each present
 * badge's box width. Returns 0 when no badges are rendered so the slug
 * column reclaims the space. The action-running hint, when present,
 * sits as the leftmost slot inside the cluster.
 */
export function badgeClusterCells(
  row: WorktreeRow,
  actionRunning: boolean,
  activeHarnessId: HarnessId | undefined,
): number {
  const isDeployed = row.fields.deploy.data ?? false;
  const showChecks =
    !!row.pr && row.pr.state === "OPEN" && row.pr.checks !== "none";
  // Action and harness-glyph slots coexist (e.g. a row running an
  // action while hosting a live session shows both glyphs).
  const showSessionSlot = activeHarnessId !== undefined;
  const conflict = hasConflict(row);
  const hasAnyBadge =
    actionRunning ||
    showSessionSlot ||
    conflict ||
    !!(row.pr || row.mq || isDeployed);
  if (!hasAnyBadge) return 0;
  let cells = 2; // leading gap
  if (actionRunning) cells += 2;
  if (showSessionSlot) cells += 2;
  if (conflict) cells += 2;
  if (rabbitHint(row)) cells += 2;
  if (reviewHint(row)) cells += 2;
  // The PR-state slot doubles as the merge-queue slot: a queued PR
  // swaps the PR glyph for the mq indicator and the slot widens to 4
  // (icon + space + position digit); otherwise it's the 2-cell PR icon.
  if (row.mq) cells += 4;
  else if (row.pr) cells += 2;
  if (showChecks) cells += 2;
  if (isDeployed) cells += 2;
  return cells;
}

/**
 * Color the merge-queue indicator by state. Green = about to land,
 * yellow = waiting on checks or behind others, red = blocked/failed.
 */
function mqColor(state: MergeQueueState): string {
  switch (state) {
    case "MERGEABLE":
      return theme.ok;
    case "AWAITING_CHECKS":
    case "QUEUED":
      return theme.warn;
    case "UNMERGEABLE":
    case "LOCKED":
      return theme.err;
    default:
      return theme.fgDim;
  }
}

/**
 * "<mq-glyph> N" for a merge-queue position: nerd-font merge-queue
 * octicon + 1-based position (`+` if there are ≥10 ahead). Rendered in
 * place of the PR-state glyph when the PR is queued, so the slot widens
 * to 4 cells (2-cell icon + 1-cell space + 1-cell digit). Only called
 * when `row.mq` exists; the empty fallback is defensive.
 */
function mqGlyph(row: WorktreeRow): string {
  const mq = row.mq;
  if (!mq) return "";
  const pos = mq.position;
  const digit = pos >= 10 ? "+" : String(pos);
  return `${NF.mergeQueue} ${digit}`;
}

/**
 * Tiny CI rollup glyph rendered next to the PR badge. Glyph/color from
 * `checkBadge`; only shown for live PRs — after merge/close the check
 * state is noise. Falls back to the empty 2-cell slot for the quiet
 * `none` state so the cluster stays aligned.
 */
function checkGlyph(row: WorktreeRow): Badge {
  const pr = row.pr;
  if (!pr || pr.state !== "OPEN") return { glyph: "  ", fg: theme.fgDim };
  return checkBadge(pr.checks) ?? { glyph: "  ", fg: theme.fgDim };
}

/**
 * True when the rebase-conflict pre-flight determined HEAD won't merge
 * cleanly onto its base. Only the `conflict` verdict shows a glyph —
 * `clean` and `unknown` (unresolved ref, ancient git, still loading)
 * stay silent, so the cluster reads "absence == fine".
 */
function hasConflict(row: WorktreeRow): boolean {
  return row.fields.conflict.data?.status === "conflict";
}

/**
 * Human-review hint. Glyph/color from `reviewBadge`; gated to OPEN
 * non-draft PRs (mirrors `reviewLabel` + `buildPrSegments` in pr.tsx).
 */
function reviewHint(row: WorktreeRow): Badge | null {
  const pr = row.pr;
  if (!pr || pr.state !== "OPEN" || pr.isDraft) return null;
  return reviewBadge(pr.review);
}

/**
 * CodeRabbit hint. Glyph/color from `rabbitBadge`; same OPEN/non-draft
 * gate as review. Draft-hide also sidesteps the "review skipped" →
 * mis-classified-as-clean issue (see `buildPrSegments` in pr.tsx).
 */
function rabbitHint(row: WorktreeRow): Badge | null {
  const pr = row.pr;
  if (!pr || pr.state !== "OPEN" || pr.isDraft) return null;
  return rabbitBadge(pr.rabbit);
}

/**
 * Compact badge cluster: only present badges render, butted up
 * right-aligned. Each badge sits in an explicit-width box so opentui's
 * flex layout reserves the right number of buffer cells regardless of
 * whether `Bun.stringWidth` and the native renderer agree on the icon's
 * width. The leading 2-space gap visually separates the cluster from
 * the label to its left; the whole cluster is omitted (null) when no
 * badges are present so the label can extend into the freed space.
 * Archived rows dim every slot except the action/session glyphs —
 * running work or a live session against an archived worktree is worth
 * seeing.
 */
export function BadgeCluster({
  row,
  actionRunning,
  activeHarnessId,
  sessionState,
}: {
  row: WorktreeRow;
  /** Whether a tracked headless action is currently running on this slug. */
  actionRunning: boolean;
  /** The harness of this slug's active (F12-target) session, or
   *  undefined when no session is live. */
  activeHarnessId: HarnessId | undefined;
  /** Derived state of that active session — tints the harness glyph
   *  with `stateColor` when known, else the harness brand color. */
  sessionState: DerivedState | undefined;
}) {
  const prb = row.pr
    ? prStateBadge(row.pr)
    : { glyph: "  ", fg: theme.fgDim };
  const prFg = row.archived ? theme.fgDim : prb.fg;
  const c = checkGlyph(row);
  const checkFg = row.archived ? theme.fgDim : c.fg;
  const isDeployed = row.fields.deploy.data ?? false;
  const deployFg = row.archived
    ? theme.fgDim
    : isDeployed
      ? theme.warn
      : theme.fgDim;
  const mqFg = row.archived || !row.mq ? theme.fgDim : mqColor(row.mq.state);
  const mqText = mqGlyph(row);
  const showChecks =
    row.pr && row.pr.state === "OPEN" && row.pr.checks !== "none";
  const rabbit = rabbitHint(row);
  const review = reviewHint(row);
  const rabbitFg = row.archived || !rabbit ? theme.fgDim : rabbit.fg;
  const reviewFg = row.archived || !review ? theme.fgDim : review.fg;
  // Two independent 2-cell slots: action (comment glyph, green) and
  // harness glyph (tinted with the harness's own color). They coexist
  // so a row running an action while hosting a live session shows both.
  const showSessionSlot = activeHarnessId !== undefined;
  const conflict = hasConflict(row);
  const hasAnyBadge =
    actionRunning ||
    showSessionSlot ||
    conflict ||
    !!(row.pr || row.mq || isDeployed);
  if (!hasAnyBadge) return null;
  return (
    <box flexShrink={0} flexDirection="row">
      <text>  </text>
      {actionRunning ? (
        <box width={2} flexShrink={0}>
          <text fg={theme.ok}>{NF.comment}</text>
        </box>
      ) : null}
      {/* Rebase-conflict warning — sits just left of the PR-signal
          sub-cluster so it reads as "this branch/PR won't land cleanly".
          Dimmed on archived rows like the rest of the cluster. */}
      {conflict ? (
        <box width={2} flexShrink={0}>
          <text fg={row.archived ? theme.fgDim : theme.err}>{NF.conflict}</text>
        </box>
      ) : null}
      {/* Ephemeral / scattered badges are left-anchored so they don't
          displace the PR-status run on the right, ordered by
          transience: action-running → stage/deploy bolt. */}
      {isDeployed ? (
        <box width={2} flexShrink={0}>
          <text fg={deployFg}>{NF.bolt}</text>
        </box>
      ) : null}
      {showSessionSlot && activeHarnessId ? (
        <box width={2} flexShrink={0}>
          <text
            fg={
              sessionState
                ? stateColor(activeHarnessId, sessionState)
                : getHarness(activeHarnessId).color
            }
          >
            {getHarness(activeHarnessId).glyph}
          </text>
        </box>
      ) : null}
      {/* CR and review hints sit immediately to the left of the PR icon
          so the eye reads "[cr] [review] [pr]" as a tight cluster of
          "what's the state of this PR" signals. Each is omitted entirely
          when its hint helper returns null, so a row with no review
          activity collapses cleanly instead of leaving dead space. */}
      {rabbit ? (
        <box width={2} flexShrink={0}>
          <text fg={rabbitFg}>{rabbit.glyph}</text>
        </box>
      ) : null}
      {review ? (
        <box width={2} flexShrink={0}>
          <text fg={reviewFg}>{review.glyph}</text>
        </box>
      ) : null}
      {/* PR-state slot, doubling as the merge-queue slot: a queued PR
          shows the mq indicator (icon + position) in place of the PR
          glyph, widening to 4 cells. */}
      {row.mq ? (
        <box width={4} flexShrink={0}>
          <text fg={mqFg}>{mqText}</text>
        </box>
      ) : row.pr ? (
        <box width={2} flexShrink={0}>
          <text fg={prFg}>{prb.glyph}</text>
        </box>
      ) : null}
      {showChecks ? (
        <box width={2} flexShrink={0}>
          <text fg={checkFg}>{c.glyph}</text>
        </box>
      ) : null}
    </box>
  );
}
