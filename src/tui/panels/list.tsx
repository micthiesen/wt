/**
 * Worktree row list (left pane).
 *
 * Status, PR-state, check, and merge-queue glyphs all come from
 * `tui/badges.ts` so this panel and the details pane stay in
 * lockstep — see that file's header for the icon/color rules.
 * Anything new that should read consistently across both panels
 * belongs in `badges.ts` first, not here.
 */
import { Fragment, memo, useEffect, useRef } from "react";
import { TextAttributes } from "@opentui/core";
import type { ScrollBoxRenderable } from "@opentui/core";

import {
  type Badge,
  checkBadge,
  prStateBadge,
  rabbitBadge,
  reviewBadge,
  statusBadge,
} from "../badges.ts";
import { NF } from "../icons.ts";
import { Spinner } from "../spinner.tsx";
import { truncateEnd } from "../text.ts";
import { theme } from "../theme.ts";
import { getHarness } from "../../core/harness/index.ts";
import type { HarnessId } from "../../core/harness/index.ts";
import type { DerivedState } from "../../core/claude-status.ts";
import { STATE_FG } from "../claude-state.ts";
import { isMergeQueued } from "../../core/graphite-api.ts";
import type { ReviewRequestPr } from "../../core/github.ts";
import { capitalizeFirst, slugLabel } from "../../core/stage.ts";
import { StatusKind } from "../../core/types.ts";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";

type Props = {
  rows: WorktreeRow[];
  /**
   * PRs the user has been asked to review. Pinned in their own section
   * between the active worktrees and the archived block. Not worktrees
   * (no local checkout, no per-slug state) so they render with a
   * stripped-down row component and no badge cluster.
   */
  reviewRequests: readonly ReviewRequestPr[];
  /**
   * Combined cursor index across `activeRows + reviewRequests +
   * archivedRows` in render order. Parent owns the unification so
   * navigation handlers can pick the right item type by index without
   * the list panel re-implementing the ordering.
   */
  selectedIndex: number;
  width: number;
  activeTails: Set<string>;
  /** Slugs with an in-flight `claude -p` action. Renders the comment
   *  glyph in the badge cluster while running. */
  activeActions: ReadonlySet<string>;
  /**
   * Per-slug first-live harness in HARNESSES order. Present only
   * when at least one live AI tmux session exists on the slug.
   * Drives the harness-glyph badge in the list pane, tinted with
   * that harness's own color.
   */
  aiLiveHarnessBySlug: ReadonlyMap<string, HarnessId>;
  /**
   * Per-slug aggregate Claude session state. When present (and the live
   * harness is Claude) the CC glyph is tinted with `STATE_FG[state]`
   * instead of the harness brand color, so the badge reads working /
   * asking / waiting / unknown at a glance. Registry-only, so codex /
   * opencode slugs are absent here and keep their brand color.
   */
  aiStateBySlug: ReadonlyMap<string, DerivedState>;
  /**
   * Per-stack-section AI-derived display label, keyed by the stored
   * section name (`stack: 1234`). When present, the section's divider
   * shows the AI title instead of the storage name. Missing entries
   * fall back to the storage name silently — AI unconfigured / call
   * pending / call failed all look identical.
   */
  stackSectionLabels: ReadonlyMap<string, string>;
  isLoading: boolean;
  filter: string;
};

/**
 * Status indicator — always reflects the actual worktree status (busy /
 * missing / gone / merged / dirty / clean). Background refetch state
 * is hinted via the spinner badge in the right cluster instead, so it
 * doesn't masquerade as a primary status. Archived rows render dim.
 */
function StatusMarker({ row }: { row: WorktreeRow }) {
  const base = statusBadge(row.status);
  const fg = row.archived ? theme.fgDim : base.fg;
  return <text fg={fg}>{base.glyph}</text>;
}

/**
 * Background refetch is in flight. Suppressed during user-initiated busy
 * ops (those have their own loud status icon and tail in the activity
 * pane — adding a refresh hint would be redundant noise).
 */
function isRefreshing(row: WorktreeRow): boolean {
  return row.anyFetching && row.status.kind !== StatusKind.Busy;
}

/**
 * Row label text. Prefers the LLM-authored `brief` (caveman-talk noun
 * phrase) over the longer `title`, since the list column is tight —
 * after the badge cluster on a busy row the slug area can drop to ~20
 * chars. The issue-tracker prefix is stripped (`ENG-4926` → `4926`)
 * because it's constant for a given `id_pattern` and pure noise here;
 * the full ID is preserved in the details pane via the panel title.
 * First char is capitalized to match PR-title convention even when the
 * LLM emits lowercase.
 */
function rowLabel(row: WorktreeRow): string {
  const { id } = slugLabel(row.wt.slug);
  const numId = id ? id.replace(/^[A-Z]+-/, "") : null;
  const text = capitalizeFirst(row.brief ?? row.title);
  return numId ? `${numId}: ${text}` : text;
}

/**
 * Cells the badge cluster occupies for a given row. Mirrors the
 * width-prop layout in the JSX below: 2-cell leading gap + each present
 * badge's box width. Returns 0 when no badges are rendered so the slug
 * column reclaims the space. The action-running hint, when present,
 * sits as the leftmost slot inside the cluster, before the refresh
 * spinner.
 */
function badgeClusterCells(
  row: WorktreeRow,
  actionRunning: boolean,
  activeHarnessId: HarnessId | undefined,
): number {
  const isDeployed = row.fields.deploy.data ?? false;
  const showChecks =
    !!row.pr && row.pr.state === "OPEN" && row.pr.checks !== "none";
  const refreshing = isRefreshing(row);
  // Action and harness-glyph slots coexist (e.g. a row running an
  // action while hosting a live session shows both glyphs).
  const showSessionSlot = activeHarnessId !== undefined;
  const hasAnyBadge =
    actionRunning ||
    showSessionSlot ||
    refreshing ||
    !!(row.pr || isDeployed);
  if (!hasAnyBadge) return 0;
  let cells = 2; // leading gap
  if (actionRunning) cells += 2;
  if (showSessionSlot) cells += 2;
  if (refreshing) cells += 2;
  if (rabbitHint(row)) cells += 2;
  if (reviewHint(row)) cells += 2;
  if (row.pr) cells += 2;
  if (showChecks) cells += 2;
  if (isDeployed) cells += 2;
  return cells;
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

const RowView = memo(function RowView({
  row,
  selected,
  isTailing,
  actionRunning,
  activeHarnessId,
  sessionState,
  panelWidth,
  stackParentAbove,
}: {
  row: WorktreeRow;
  selected: boolean;
  isTailing: boolean;
  /** Whether a `claude -p` action is currently running on this slug. */
  actionRunning: boolean;
  /** The first live harness (in HARNESSES order) on this slug, or
   *  undefined when no live AI sessions exist. Renders the harness
   *  glyph in the badge cluster when defined. */
  activeHarnessId: HarnessId | undefined;
  /** Aggregate Claude session state for this slug. Tints the CC glyph
   *  with `STATE_FG[state]` when the active harness is Claude; otherwise
   *  the glyph falls back to the harness brand color. */
  sessionState: DerivedState | undefined;
  panelWidth: number;
  /**
   * True when the row immediately above is the worktree this one is
   * stacked on (commit-signal), in the same section, not archived.
   * When true, the PR badge slot renders the "↑" angles-up glyph
   * instead of the usual PR-state icon — a quiet hint that manual
   * order matches the actual stack. Width and color are unchanged so
   * the badge cluster stays aligned with rows that show the PR icon.
   */
  stackParentAbove: boolean;
}) {
  const bg = selected ? theme.rowSelectedBg : undefined;
  // Archived rows render dim (unless selected, where we still want
  // contrast). Badges also render dim so the eye skips over them.
  const slugFg = row.archived
    ? selected
      ? theme.fg
      : theme.fgDim
    : selected
      ? theme.fgBright
      : theme.fg;
  // When the PR is armed in the Graphite merge queue, the PR slot stops
  // showing the open/draft state and instead reads as "merging": the
  // merge-queue glyph in magenta. Mirrors the details-pane `queued to
  // merge` pill so both panes agree the next thing that happens is a
  // merge, not more review.
  const queued = !!row.pr && isMergeQueued(row.mergeability);
  const prb = row.pr
    ? prStateBadge(row.pr)
    : { glyph: "  ", fg: theme.fgDim };
  const prFg = row.archived ? theme.fgDim : queued ? theme.info : prb.fg;
  const c = checkGlyph(row);
  const checkFg = row.archived ? theme.fgDim : c.fg;
  const deployFg = row.archived
    ? theme.fgDim
    : (row.fields.deploy.data ?? false)
      ? theme.warn
      : theme.fgDim;
  const isDeployed = row.fields.deploy.data ?? false;
  const showChecks =
    row.pr && row.pr.state === "OPEN" && row.pr.checks !== "none";
  const refreshing = isRefreshing(row);
  const rabbit = rabbitHint(row);
  const review = reviewHint(row);
  const rabbitFg = row.archived || !rabbit ? theme.fgDim : rabbit.fg;
  const reviewFg = row.archived || !review ? theme.fgDim : review.fg;
  // Two independent 2-cell slots: action (comment glyph, green) and
  // harness glyph (tinted with the harness's own color). They
  // coexist so a row running an action while hosting a live session
  // shows both. Both slots stay lit on archived rows — running work
  // or a live session against an archived worktree is worth seeing.
  const showSessionSlot = activeHarnessId !== undefined;
  const hasAnyBadge =
    actionRunning ||
    showSessionSlot ||
    refreshing ||
    !!(row.pr || isDeployed);
  // OpenTUI `attributes` is a bitmask over TextAttributes. Combine BOLD
  // (selection) and ITALIC (tailing) so both indicators survive when
  // a row is both selected and being tailed.
  const slugAttrs =
    (selected ? TextAttributes.BOLD : 0) |
    (isTailing ? TextAttributes.ITALIC : 0);
  return (
    <box
      id={row.wt.slug}
      flexDirection="row"
      backgroundColor={bg}
      paddingLeft={1}
      paddingRight={1}
    >
      <box flexShrink={0} flexDirection="row">
        {/* Mirror the right-cluster pattern: width=2 box for the icon,
            then a width=1 box for the gap. Same shape that produces
            tight left-aligned icons over there. */}
        <box width={2} flexShrink={0}>
          <StatusMarker row={row} />
        </box>
        <box width={1} flexShrink={0}>
          <text> </text>
        </box>
      </box>
      <box flexGrow={1} flexShrink={1} overflow="hidden">
        {/* Truncation lives in JS, not opentui's native `truncate`,
            because the native path middle-clips with `…`. We want the
            head intact (it's the most distinctive part: "ENG-1234: "
            and the leading words of the title). Width budget = panel
            width − borders(2) − row padding(2) − marker+gap(3) − badge
            cluster. */}
        <text fg={slugFg} attributes={slugAttrs} wrapMode="none">
          {truncateEnd(rowLabel(row), Math.max(0, panelWidth - 7 - badgeClusterCells(row, actionRunning, activeHarnessId)))}
        </text>
      </box>
      {/* Compact badge cluster: only render present badges, butted up
          right-aligned. Each badge sits in an explicit-width box so
          opentui's flex layout reserves the right number of buffer
          cells regardless of whether `Bun.stringWidth` and the native
          renderer agree on the icon's width. PR/CI/deploy are 2-cell
          icons → width=2; MQ has a space between icon and digit → 4.
          The leading 2-space gap visually separates the cluster from
          the slug, mirroring the gap after the status marker. The
          whole cluster is omitted when no badges are present so the
          slug can extend into the freed space without a dead gap. */}
      {hasAnyBadge ? (
        <box flexShrink={0} flexDirection="row">
          <text>  </text>
          {/* Refresh spinner is ALWAYS the leftmost slot in the badge
              cluster. It's the most ephemeral signal (a row's data is
              actively being refetched), so anchoring it on the left
              gives it a stable, unmissable position regardless of which
              other badges happen to be present. Don't reorder. */}
          {refreshing ? (
            <box width={2} flexShrink={0}>
              <Spinner fg={theme.fgDim} />
            </box>
          ) : null}
          {actionRunning ? (
            <box width={2} flexShrink={0}>
              <text fg={theme.ok}>{NF.comment}</text>
            </box>
          ) : null}
          {/* Ephemeral / scattered badges are left-anchored so they
              don't displace the PR-status run on the right, ordered by
              transience: spinner → action-running → stage/deploy bolt. */}
          {isDeployed ? (
            <box width={2} flexShrink={0}>
              <text fg={deployFg}>{NF.bolt}</text>
            </box>
          ) : null}
          {showSessionSlot && activeHarnessId ? (
            <box width={2} flexShrink={0}>
              <text
                fg={
                  activeHarnessId === "claude" && sessionState
                    ? STATE_FG[sessionState]
                    : getHarness(activeHarnessId).color
                }
              >
                {getHarness(activeHarnessId).glyph}
              </text>
            </box>
          ) : null}
          {/* CR and review hints sit immediately to the left of the
              PR icon so the eye reads "[cr] [review] [pr]" as a
              tight cluster of "what's the state of this PR" signals.
              Each is omitted entirely when its hint helper returns
              null, so a row with no review activity collapses cleanly
              instead of leaving dead space. */}
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
          {row.pr ? (
            <box width={2} flexShrink={0}>
              <text fg={prFg}>
                {queued
                  ? NF.mergeQueue
                  : stackParentAbove
                    ? NF.anglesUp
                    : prb.glyph}
              </text>
            </box>
          ) : null}
          {showChecks ? (
            <box width={2} flexShrink={0}>
              <text fg={checkFg}>{c.glyph}</text>
            </box>
          ) : null}
        </box>
      ) : null}
    </box>
  );
});

/**
 * Tiny CI rollup for a review-request row — same `checkBadge` as the
 * worktree row, just read from the standalone `ReviewRequestPr.checks`
 * rollup (no `PullRequest` shape). Empty slot for the quiet state.
 */
function reviewCheckGlyph(checks: ReviewRequestPr["checks"]): Badge {
  return checkBadge(checks) ?? { glyph: "  ", fg: theme.fgDim };
}

/**
 * Row in the "review requests" pinned section. Not a worktree — no
 * slug, no per-slug state, no badge cluster. Just the PR icon (open or
 * draft), a label (`owner/repo#N · title`), and a check-rollup glyph
 * on the right when CI is reporting. Selection still highlights with
 * the same bg as worktree rows so j/k navigation feels unified.
 */
const ReviewRequestRowView = memo(function ReviewRequestRowView({
  pr,
  selected,
  panelWidth,
}: {
  pr: ReviewRequestPr;
  selected: boolean;
  panelWidth: number;
}) {
  const bg = selected ? theme.rowSelectedBg : undefined;
  const prFg = pr.isDraft ? theme.fgDim : theme.accentAlt;
  const prGlyph = pr.isDraft ? NF.prDraft : NF.prOpen;
  const check = reviewCheckGlyph(pr.checks);
  const showChecks = pr.checks !== "none";
  // PR title only; repo + number live in the details pane.
  const label = capitalizeFirst(pr.title);
  // Match worktree row width budget: borders(2) + paddingLeft+right(2)
  // + leading PR-icon slot(3) + trailing check slot when present(2).
  const trailingCells = showChecks ? 2 + 2 : 0;
  const budget = Math.max(0, panelWidth - 7 - trailingCells);
  const slugAttrs = selected ? TextAttributes.BOLD : 0;
  const slugFg = selected ? theme.fgBright : theme.fg;
  return (
    <box id={pr.url} flexDirection="row" backgroundColor={bg} paddingLeft={1} paddingRight={1}>
      <box flexShrink={0} flexDirection="row">
        <box width={2} flexShrink={0}>
          <text fg={prFg}>{prGlyph}</text>
        </box>
        <box width={1} flexShrink={0}>
          <text> </text>
        </box>
      </box>
      <box flexGrow={1} flexShrink={1} overflow="hidden">
        <text fg={slugFg} attributes={slugAttrs} wrapMode="none">
          {truncateEnd(label, budget)}
        </text>
      </box>
      {showChecks ? (
        <box flexShrink={0} flexDirection="row">
          <text>  </text>
          <box width={2} flexShrink={0}>
            <text fg={check.fg}>{check.glyph}</text>
          </box>
        </box>
      ) : null}
    </box>
  );
});

/**
 * Section divider. The `stack` variant signals an auto-managed
 * stack section: double-line rule chars with `╔═ … ═╗` corner
 * brackets in the accentAlt color, so the section reads as an
 * enclosed structural block rather than a plain rule.
 */
function Divider({
  label,
  width,
  variant = "manual",
}: {
  label: string;
  width: number;
  variant?: "manual" | "stack";
}) {
  // Leave room for padding (border+paddingLeft+paddingRight roughly 4
  // cells) so the rule doesn't bleed past the panel edge.
  const inner = Math.max(0, width - 4);
  if (variant === "stack") {
    // Layout: `══` + ` label ` + `═══…═`. Same shape as the manual
    // divider but with double-line rule chars in accentAlt.
    const labelStr = ` ${label} `;
    const overhead = 2 + labelStr.length;
    const trailLen = Math.max(0, inner - overhead);
    const trail = "═".repeat(trailLen);
    return (
      <box flexDirection="row" height={1} paddingLeft={1} paddingRight={1}>
        <text fg={theme.accentAlt} wrapMode="none">══</text>
        <text fg={theme.fg} wrapMode="none">{labelStr}</text>
        <text fg={theme.accentAlt} wrapMode="none">{trail}</text>
      </box>
    );
  }
  const labelStr = ` ${label} `;
  const padding = Math.max(0, inner - labelStr.length - 2);
  const trail = "─".repeat(padding);
  // height={1} + wrapMode="none" keep the rule on a single line. Inside
  // the scrollbox the vertical scrollbar steals a column when the list
  // overflows, which makes the computed-width trail one cell too wide;
  // without these it text-wraps to a phantom blank line under the header.
  return (
    <box flexDirection="row" height={1} paddingLeft={1} paddingRight={1}>
      <text fg={theme.borderDim} wrapMode="none">──</text>
      <text fg={theme.fgDim} wrapMode="none">{labelStr}</text>
      <text fg={theme.borderDim} wrapMode="none">{trail}</text>
    </box>
  );
}

export function WorktreeList({ rows, reviewRequests, selectedIndex, width, activeTails, activeActions, aiLiveHarnessBySlug, aiStateBySlug, stackSectionLabels, isLoading, filter }: Props) {
  const firstArchivedIndex = rows.findIndex((r) => r.archived);
  const hasArchived = firstArchivedIndex !== -1;
  const activeRows = hasArchived ? rows.slice(0, firstArchivedIndex) : rows;
  const archivedRows = hasArchived ? rows.slice(firstArchivedIndex) : [];
  const hasReviewRequests = reviewRequests.length > 0;
  // Index offsets into the combined cursor space owned by the parent.
  const reviewOffset = activeRows.length;
  const archivedOffset = reviewOffset + reviewRequests.length;
  // Keep the selected row scrolled into view. The whole list (active +
  // review-requests + archived) lives in one scrollbox, so the follow
  // covers every row. scrollChildIntoView is a no-op when the row is
  // already visible. Child ids: the worktree slug for active/archived
  // rows, the PR url for review-request rows.
  const listRef = useRef<ScrollBoxRenderable>(null);
  const selectedChildId =
    selectedIndex < reviewOffset
      ? activeRows[selectedIndex]?.wt.slug
      : selectedIndex < archivedOffset
        ? reviewRequests[selectedIndex - reviewOffset]?.url
        : archivedRows[selectedIndex - archivedOffset]?.wt.slug;
  // Depend on `rows`/`reviewRequests` (both identity-stable per
  // useWorktreeRows / the query layer) as well as the selected id, so a
  // reflow under a stationary selection — a row inserted above, a
  // section divider appearing, an active↔archived split shift — re-runs
  // the follow instead of leaving the cursor drifted off-screen.
  useEffect(() => {
    if (selectedChildId) listRef.current?.scrollChildIntoView(selectedChildId);
  }, [selectedChildId, rows, reviewRequests]);
  return (
    <box
      flexDirection="column"
      width={width}
      flexShrink={0}
      border
      borderStyle="single"
      borderColor={theme.border}
      title=" worktrees "
      titleAlignment="left"
      paddingTop={0}
    >
      {rows.length === 0 && !hasReviewRequests ? (
        <box padding={1}>
          {isLoading ? (
            <text fg={theme.fgDim}>Loading worktrees...</text>
          ) : filter ? (
            <text fg={theme.fgDim}>
              No matches for <span fg={theme.fg}>/{filter}</span>
            </text>
          ) : (
            <>
              <text fg={theme.fgDim}>No worktrees.</text>
              <text> </text>
              <text fg={theme.fgDim}>Press </text>
              <text fg={theme.accent} attributes={1}>
                n
              </text>
              <text fg={theme.fgDim}> to create one.</text>
            </>
          )}
        </box>
      ) : (
        <>
          {rows.length === 0 ? (
            // No worktrees but review-requests are loaded — still surface
            // the new-worktree hint so the user isn't left wondering where
            // the worktree column went. The PR section renders below.
            <box padding={1} flexDirection="row">
              <text fg={theme.fgDim}>No worktrees. Press </text>
              <text fg={theme.accent} attributes={1}>
                n
              </text>
              <text fg={theme.fgDim}> to create one.</text>
            </box>
          ) : null}
          {/* The whole list scrolls as one — active worktrees, review
              requests, and the archived block all live in this scrollbox.
              `minHeight={0}` lets it shrink to the flex-allotted height
              instead of growing to fit its content (the default
              `min-height: auto`), which is what makes it actually scroll
              rather than shove the layout. */}
          <scrollbox ref={listRef} scrollY flexGrow={1} minHeight={0}>
          {activeRows.map((row, i) => {
            // Section transition: a blank line above the divider, then the
            // divider, then the section's rows immediately — no blank
            // between a header and its worktrees. Unsectioned rows at the
            // top get no header — implicit "inbox". When the very first row
            // belongs to a section (no inbox rows above it), the leading
            // blank still renders so the list opens with breathing room
            // above the first header rather than butting it to the border.
            const prev = i > 0 ? activeRows[i - 1] : undefined;
            const sectionChanged = (prev?.section ?? null) !== row.section;
            const showDivider = sectionChanged && row.section !== null;
            // Stack-parent hint fires only when the row immediately
            // above is the actual parent worktree, sits in the same
            // section, and isn't archived. Honors manual ordering
            // without enforcing it: when the user happens to place a
            // stack contiguously, the "↑" lights up; otherwise the
            // normal PR badge stays put. The condition explicitly
            // requires `row.pr` because the hint replaces the PR
            // glyph slot — without a PR there's no slot to swap.
            const stackParentAbove =
              !!row.pr &&
              !!row.stackedOn &&
              !!prev &&
              !sectionChanged &&
              !prev.archived &&
              prev.wt.slug === row.stackedOn.slug;
            return (
              <Fragment key={row.wt.slug}>
                {showDivider ? (
                  <>
                    <box height={1} flexShrink={0} />
                    <Divider
                      label={
                        row.sectionIsStack
                          ? stackSectionLabels.get(row.section!) ?? row.section!
                          : row.section!
                      }
                      width={width}
                      variant={row.sectionIsStack ? "stack" : "manual"}
                    />
                  </>
                ) : null}
                <RowView
                  row={row}
                  selected={i === selectedIndex}
                  isTailing={activeTails.has(row.wt.slug)}
                  actionRunning={activeActions.has(row.wt.slug)}
                  activeHarnessId={aiLiveHarnessBySlug.get(row.wt.slug)}
                  sessionState={aiStateBySlug.get(row.wt.slug)}
                  panelWidth={width}
                  stackParentAbove={stackParentAbove}
                />
              </Fragment>
            );
          })}
          {hasReviewRequests ? (
            <>
              {activeRows.length > 0 ? (
                <box height={1} flexShrink={0} />
              ) : null}
              <Divider label="Review Requests" width={width} />
              {reviewRequests.map((pr, i) => {
                const globalIndex = reviewOffset + i;
                return (
                  <ReviewRequestRowView
                    key={pr.url}
                    pr={pr}
                    selected={globalIndex === selectedIndex}
                    panelWidth={width}
                  />
                );
              })}
            </>
          ) : null}
          {hasArchived ? (
            <>
              {activeRows.length > 0 || hasReviewRequests ? (
                // Flex spacer: pushes the archived block to the bottom of
                // the viewport when the list is short, but collapses to a
                // 1-row gap (minHeight) once content overflows, so archived
                // just scrolls into place instead of staying pinned. Relies
                // on the scrollbox content box's default `minHeight: 100%` —
                // free space exists only while content is shorter than the
                // viewport, so flexGrow has room to grow only then.
                <box flexGrow={1} flexShrink={0} minHeight={1} />
              ) : null}
              <Divider label="Archived" width={width} />
              {archivedRows.map((row, i) => {
                const globalIndex = archivedOffset + i;
                // Archived rows never show the stack hint — the
                // archive block is a flat list with a hard divider
                // above it, so any "above me" relationship across
                // that divider would be misleading.
                return (
                  <RowView
                    key={row.wt.slug}
                    row={row}
                    selected={globalIndex === selectedIndex}
                    isTailing={activeTails.has(row.wt.slug)}
                    actionRunning={activeActions.has(row.wt.slug)}
                    activeHarnessId={aiLiveHarnessBySlug.get(row.wt.slug)}
                    sessionState={aiStateBySlug.get(row.wt.slug)}
                    panelWidth={width}
                    stackParentAbove={false}
                  />
                );
              })}
            </>
          ) : null}
          </scrollbox>
        </>
      )}
    </box>
  );
}
