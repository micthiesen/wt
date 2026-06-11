/**
 * Worktree row list (left pane).
 *
 * Status, PR-state, check, and merge-queue glyphs all come from
 * `tui/badges.ts` so this panel and the details pane stay in
 * lockstep — see that file's header for the icon/color rules.
 * Anything new that should read consistently across both panels
 * belongs in `badges.ts` first, not here.
 */
import { Fragment, memo, useEffect, useRef, type RefObject } from "react";
import { TextAttributes } from "@opentui/core";
import type { ScrollBoxRenderable } from "@opentui/core";

import { type Badge, checkBadge, statusBadge } from "../badges.ts";
import { BadgeCluster, badgeClusterCells } from "../badge-cluster.tsx";
import { NF } from "../icons.ts";
import { useScrollbarNoFlash } from "../hooks/useScrollbarNoFlash.ts";
import { truncateEnd } from "../text.ts";
import { theme } from "../theme.ts";
import type { HarnessId } from "../../core/harness/index.ts";
import type { DerivedState } from "../../core/claude-status.ts";
import type { ReviewRequestPr } from "../../core/github.ts";
import { capitalizeFirst, slugLabel } from "../../core/stage.ts";
import { STACK_CONNECTOR, stackOrdinalLabel } from "../../core/stack-layout.ts";
import type { ActiveSessionGlyph } from "../hooks/useHarnessSessions.ts";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";

/**
 * One entry in the ACTIVE portion of the list. Either a worktree row, or a
 * folded section collapsed to a single selectable header line. The parent
 * (`app.tsx`) builds this so the cursor model and the render share one source
 * of truth — a folded section is one cursor stop, not N hidden rows.
 */
export type ListActiveItem =
  | { kind: "wt"; row: WorktreeRow }
  | {
      kind: "section";
      /** Synthetic section key (`stackSectionKey(stackId)` or a manual name). */
      sectionKey: string;
      isStack: boolean;
      /** Resolved header label (stack: issue + AI title; manual: the name). */
      label: string;
      /** The collapsed member rows (for the count + the detail-pane summary). */
      rows: WorktreeRow[];
    };

/**
 * Imperative scroll control the parent's j/k handler calls when the cursor is
 * already at the first/last item — scroll the whole pane to the very top/bottom
 * so trailing blank space + the review/archived headers below the last row
 * become reachable (the cursor can't land on them).
 */
export type ListScrollHandle = { toEdge: (dir: "top" | "bottom") => void };

type Props = {
  /**
   * Active worktrees + folded section headers, in render order. Folded
   * sections appear as one `section` item; expanded ones as their `wt` rows.
   */
  items: readonly ListActiveItem[];
  /** Populated with the pane's scroll-to-edge control (see `ListScrollHandle`). */
  scrollHandle?: RefObject<ListScrollHandle | null>;
  /** The archived block (never folded). */
  archivedRows: readonly WorktreeRow[];
  /**
   * PRs the user has been asked to review. Pinned in their own section
   * between the active worktrees and the archived block. Not worktrees
   * (no local checkout, no per-slug state) so they render with a
   * stripped-down row component and no badge cluster.
   */
  reviewRequests: readonly ReviewRequestPr[];
  /**
   * Combined cursor index across `items + reviewRequests + archivedRows` in
   * render order. Parent owns the unification so navigation handlers can pick
   * the right item type by index without the list panel re-implementing it.
   */
  selectedIndex: number;
  width: number;
  activeTails: Set<string>;
  /** Slugs with an in-flight `claude -p` action. Renders the comment
   *  glyph in the badge cluster while running. */
  activeActions: ReadonlySet<string>;
  /**
   * Per-slug "active session" — the harness F12 would attach to plus its
   * derived state — for every worktree. Computed through the same
   * `computeHarnessSessions` rule as the F12 keybind and the details-pane
   * AI row (see `useActiveSessionsBySlug`), so the list glyph can't drift
   * from either. Absent when no session is live on the slug. The glyph is
   * tinted by `state` when known (any harness), else the harness's brand
   * color.
   */
  activeSessionBySlug: ReadonlyMap<string, ActiveSessionGlyph>;
  /**
   * Stack-section header label (issue + progress, with the AI title
   * woven in when resolved), keyed by the synthetic stack section key
   * (`stackSectionKey(stackId)`). Every managed stack has an entry, so
   * the divider never falls back to rendering the raw NUL-prefixed key.
   */
  stackSectionLabels: ReadonlyMap<string, string>;
  isLoading: boolean;
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
 * Glyph for the holistic-origin row pinned at the bottom of a stack: a
 * hollow diamond (the slices are solid ◆) signalling "the whole this
 * stack was carved from", kept around until it's `wt rm`'d post-split.
 */
const HOLISTIC_GLYPH = "◇";

/**
 * Left gutter for a managed-stack row, repurposing the status-marker
 * slot: a 1-cell tree connector (structural, dim) followed by the 2-cell
 * stack ordinal colored by the slice's worktree status (so dirty/merged/
 * busy still read at a glance without a separate status glyph). The
 * holistic-origin row carries no ordinal — just the distinct dim glyph.
 */
function StackGutter({ row }: { row: WorktreeRow }) {
  const info = row.stack!;
  if (info.isHolistic) {
    return (
      <box flexShrink={0} flexDirection="row">
        <box width={2} flexShrink={0}>
          <text> </text>
        </box>
        <box width={1} flexShrink={0}>
          <text fg={theme.fgDim}>{HOLISTIC_GLYPH}</text>
        </box>
        <box width={1} flexShrink={0}>
          <text> </text>
        </box>
      </box>
    );
  }
  const ordFg = row.archived ? theme.fgDim : statusBadge(row.status).fg;
  const ord = stackOrdinalLabel(info.ordinal);
  return (
    <box flexShrink={0} flexDirection="row">
      <box width={1} flexShrink={0}>
        <text fg={theme.fgDim}>{STACK_CONNECTOR[info.pos]}</text>
      </box>
      <box width={2} flexShrink={0}>
        <text fg={ordFg}>{ord}</text>
      </box>
      <box width={1} flexShrink={0}>
        <text> </text>
      </box>
    </box>
  );
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
export function rowLabel(row: WorktreeRow): string {
  // The holistic origin's title is the feature title — already on the
  // section header — so showing it again is noise. Label it for what it
  // is instead; the dim glyph + text mark it as the carved-from source.
  if (row.stack?.isHolistic) return "holistic source";
  const text = capitalizeFirst(row.brief ?? row.title);
  // Inside a stack section the issue ID is on the section header, so the
  // row drops the redundant `<id>: ` prefix and shows just the slice.
  if (row.stack) return text;
  const { id } = slugLabel(row.wt.slug);
  const numId = id ? id.replace(/^[A-Z]+-/, "") : null;
  return numId ? `${numId}: ${text}` : text;
}

const RowView = memo(function RowView({
  row,
  selected,
  isTailing,
  actionRunning,
  activeHarnessId,
  sessionState,
  panelWidth,
}: {
  row: WorktreeRow;
  selected: boolean;
  isTailing: boolean;
  /** Whether a `claude -p` action is currently running on this slug. */
  actionRunning: boolean;
  /** The harness of this slug's active (F12-target) session, or
   *  undefined when no session is live. Renders the harness glyph in the
   *  badge cluster when defined. */
  activeHarnessId: HarnessId | undefined;
  /** Derived state of that active session. Tints the harness glyph with
   *  `stateColor(harnessId, state)` (per-harness palette) when known;
   *  otherwise the glyph falls back to the harness brand color. */
  sessionState: DerivedState | undefined;
  panelWidth: number;
}) {
  const bg = selected ? theme.rowSelectedBg : undefined;
  // Archived rows and the holistic-origin row render dim (unless selected,
  // where we still want contrast). The holistic row is a kept-around
  // source, not active work, so it recedes the same way archived does.
  const dimRow = row.archived || (row.stack?.isHolistic ?? false);
  const slugFg = dimRow
    ? selected
      ? theme.fg
      : theme.fgDim
    : selected
      ? theme.fgBright
      : theme.fg;
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
      {row.stack ? (
        // Stack rows repurpose the marker slot for the tree gutter
        // (connector + ordinal). 4 cells wide (1 + 2 + gap), so the
        // label budget below accounts for one extra cell of indent.
        <StackGutter row={row} />
      ) : (
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
      )}
      <box flexGrow={1} flexShrink={1} overflow="hidden">
        {/* Truncation lives in JS, not opentui's native `truncate`,
            because the native path middle-clips with `…`. We want the
            head intact (it's the most distinctive part: "ENG-1234: "
            and the leading words of the title). Width budget = panel
            width − borders(2) − row padding(2) − left gutter (3 normal,
            4 for a stack row) − badge cluster. */}
        <text fg={slugFg} attributes={slugAttrs} wrapMode="none">
          {truncateEnd(rowLabel(row), Math.max(0, panelWidth - (row.stack ? 8 : 7) - badgeClusterCells(row, actionRunning, activeHarnessId)))}
        </text>
      </box>
      {/* Shared with the folded section/stack summaries in the details
          pane (badge-cluster.tsx) so both render identically. */}
      <BadgeCluster
        row={row}
        actionRunning={actionRunning}
        activeHarnessId={activeHarnessId}
        sessionState={sessionState}
      />
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
 * Section divider. One style for every section — manual sections and
 * auto-managed stack sections render identically (muted rule + label);
 * the stack's tree spine on its rows is what marks it as a stack, not
 * the header.
 */
function Divider({
  label,
  width,
}: {
  label: string;
  width: number;
}) {
  // Leave room for padding (border+paddingLeft+paddingRight roughly 4
  // cells) so the rule doesn't bleed past the panel edge.
  const inner = Math.max(0, width - 4);
  const labelStr = ` ${label} `;
  const padding = Math.max(0, inner - labelStr.length - 2);
  const trail = "─".repeat(padding);
  // The trail is sized for the full width, but when the list overflows the
  // vertical scrollbar steals a column, making the row one cell too wide.
  // Flex layout absorbs that: the `──` prefix is pinned (`flexShrink={0}`),
  // while the label and trail sit in `overflow="hidden"` boxes that shrink —
  // so the stolen column clips a `─` off the (much wider) trail, and the
  // label's `truncate` only ever ellipsises its TAIL, never eating the
  // leading space after `──`. height={1} + wrapMode="none" keep it one line.
  return (
    <box flexDirection="row" height={1} paddingLeft={1} paddingRight={1}>
      <box flexShrink={0}>
        <text fg={theme.borderDim} wrapMode="none">──</text>
      </box>
      <box flexShrink={1} overflow="hidden">
        <text fg={theme.fgDim} wrapMode="none" truncate>{labelStr}</text>
      </box>
      <box flexShrink={1} overflow="hidden">
        <text fg={theme.borderDim} wrapMode="none">{trail}</text>
      </box>
    </box>
  );
}

/**
 * A folded section, collapsed to one selectable header line: a `[×NN]` chip
 * with the hidden-worktree count, then the section label which truncates to a
 * native ellipsis. Highlights like a row when selected; the right detail pane
 * renders the stack/section summary while this is the cursor (TAB to expand).
 */
const FoldedSectionHeader = memo(function FoldedSectionHeader({
  item,
  selected,
}: {
  item: Extract<ListActiveItem, { kind: "section" }>;
  selected: boolean;
}) {
  const count = `[×${String(item.rows.length).padStart(2, "0")}]`;
  const labelFg = selected ? theme.fgBright : theme.fgDim;
  const attrs = selected ? TextAttributes.BOLD : 0;
  return (
    <box
      id={`section:${item.sectionKey}`}
      flexDirection="row"
      height={1}
      paddingLeft={1}
      paddingRight={1}
      backgroundColor={selected ? theme.rowSelectedBg : undefined}
    >
      <box flexShrink={0}>
        <text fg={theme.accent} wrapMode="none" attributes={attrs}>{`${count} `}</text>
      </box>
      <box flexGrow={1} flexShrink={1} overflow="hidden">
        <text fg={labelFg} wrapMode="none" truncate attributes={attrs}>
          {item.label}
        </text>
      </box>
    </box>
  );
});

export function WorktreeList({ items, archivedRows, reviewRequests, selectedIndex, width, activeTails, activeActions, activeSessionBySlug, stackSectionLabels, isLoading, scrollHandle }: Props) {
  const hasArchived = archivedRows.length > 0;
  const hasReviewRequests = reviewRequests.length > 0;
  const hasActive = items.length > 0;
  // Index offsets into the combined cursor space owned by the parent
  // (`items + reviewRequests + archivedRows`).
  const reviewOffset = items.length;
  const archivedOffset = reviewOffset + reviewRequests.length;
  // Keep the selected entry scrolled into view. The whole list (active +
  // review-requests + archived) lives in one scrollbox, so the follow
  // covers every entry. scrollChildIntoView is a no-op when it's already
  // visible. Child ids: a worktree slug, `section:<key>` for a folded
  // header, or the PR url for review-request rows.
  const listRef = useRef<ScrollBoxRenderable>(null);
  const listScrollRef = useScrollbarNoFlash(listRef);
  // Expose scroll-to-edge to the parent's j/k handler. A large `scrollBy`
  // clamps at the content edge, so this reveals trailing blank space / the
  // review + archived headers that sit below the last selectable item.
  useEffect(() => {
    if (!scrollHandle) return;
    scrollHandle.current = {
      toEdge: (dir) => listRef.current?.scrollBy(dir === "bottom" ? 9999 : -9999, "viewport"),
    };
    return () => {
      if (scrollHandle) scrollHandle.current = null;
    };
  }, [scrollHandle]);
  const selItem = selectedIndex < reviewOffset ? items[selectedIndex] : undefined;
  const selectedChildId =
    selItem !== undefined
      ? selItem.kind === "wt"
        ? selItem.row.wt.slug
        : `section:${selItem.sectionKey}`
      : selectedIndex < archivedOffset
        ? reviewRequests[selectedIndex - reviewOffset]?.url
        : archivedRows[selectedIndex - archivedOffset]?.wt.slug;
  // Depend on `items`/`reviewRequests`/`archivedRows` (identity-stable per
  // render of the parent) as well as the selected id, so a reflow under a
  // stationary selection — a row inserted above, a section folding/unfolding,
  // an active↔archived split shift — re-runs the follow instead of leaving
  // the cursor drifted off-screen.
  useEffect(() => {
    if (selectedChildId) listRef.current?.scrollChildIntoView(selectedChildId);
  }, [selectedChildId, items, reviewRequests, archivedRows]);
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
      {!hasActive && !hasArchived && !hasReviewRequests ? (
        <box padding={1}>
          {isLoading ? (
            <text fg={theme.fgDim}>Loading worktrees...</text>
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
          {!hasActive && !hasArchived ? (
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
          <scrollbox ref={listScrollRef} scrollY flexGrow={1} minHeight={0}>
          {items.map((item, i) => {
            // Section context of the previous item (a worktree's section, or a
            // folded section's key) drives the divider/blank-line transitions.
            const prev = i > 0 ? items[i - 1] : undefined;
            const prevSection = prev ? (prev.kind === "wt" ? prev.row.section : prev.sectionKey) : null;

            // A folded section collapses to one selectable header line — it IS
            // the section divider (a `[×NN]` chip + label in place of the rule),
            // and its rows are hidden. Mirror the divider's leading blank so it
            // separates from what's above.
            if (item.kind === "section") {
              return (
                <Fragment key={`section:${item.sectionKey}`}>
                  <box height={1} flexShrink={0} />
                  <FoldedSectionHeader item={item} selected={i === selectedIndex} />
                </Fragment>
              );
            }

            // Section transition: a blank line above the divider, then the
            // divider, then the section's rows immediately — no blank
            // between a header and its worktrees. The inbox at the very
            // top gets no header (implicit list); groups are freely
            // reorderable though, so when the inbox sits anywhere BELOW
            // another group its rows would visually attach to that
            // group — it gets a labeled divider like everyone else then.
            // When the very first row belongs to a section, the leading
            // blank still renders so the list opens with breathing room
            // above the first header rather than butting it to the border.
            const row = item.row;
            const sectionChanged = prevSection !== row.section;
            const showDivider =
              sectionChanged && (row.section !== null || prev !== undefined);
            return (
              <Fragment key={row.wt.slug}>
                {showDivider ? (
                  <>
                    <box height={1} flexShrink={0} />
                    <Divider
                      label={
                        row.section === null
                          ? "Inbox"
                          : row.sectionIsStack
                            ? stackSectionLabels.get(row.section) ?? row.section
                            : row.section
                      }
                      width={width}
                    />
                  </>
                ) : null}
                <RowView
                  row={row}
                  selected={i === selectedIndex}
                  isTailing={activeTails.has(row.wt.slug)}
                  actionRunning={activeActions.has(row.wt.slug)}
                  activeHarnessId={activeSessionBySlug.get(row.wt.slug)?.harnessId}
                  sessionState={activeSessionBySlug.get(row.wt.slug)?.state ?? undefined}
                  panelWidth={width}
                />
              </Fragment>
            );
          })}
          {hasReviewRequests ? (
            <>
              {hasActive ? (
                // Flex spacer at the top of the bottom group (review
                // requests + archived): pushes the whole group to the
                // bottom of the viewport when the list is short, and
                // collapses to a 1-row gap (minHeight) once content
                // overflows so the group just scrolls into place. Relies on
                // the scrollbox content box's default `minHeight: 100%` —
                // free space exists only while content is shorter than the
                // viewport. Only one such spacer renders (here when review
                // requests exist, otherwise above the archived block), so
                // the group stays contiguous instead of being split.
                <box flexGrow={1} flexShrink={0} minHeight={1} />
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
              {hasReviewRequests ? (
                // Review requests already carried the bottom-group spacer
                // above; archived just needs a 1-row separator below them.
                <box height={1} flexShrink={0} />
              ) : hasActive ? (
                // No review requests, so archived leads the bottom group —
                // it owns the flex spacer (see the review-requests block).
                <box flexGrow={1} flexShrink={0} minHeight={1} />
              ) : null}
              <Divider label="Archived" width={width} />
              {archivedRows.map((row, i) => {
                const globalIndex = archivedOffset + i;
                return (
                  <RowView
                    key={row.wt.slug}
                    row={row}
                    selected={globalIndex === selectedIndex}
                    isTailing={activeTails.has(row.wt.slug)}
                    actionRunning={activeActions.has(row.wt.slug)}
                    activeHarnessId={activeSessionBySlug.get(row.wt.slug)?.harnessId}
                    sessionState={activeSessionBySlug.get(row.wt.slug)?.state ?? undefined}
                    panelWidth={width}
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
