/**
 * Worktree row list (left pane).
 *
 * Status, PR-state, check, and merge-queue glyphs all come from
 * `tui/badges.ts` so this panel and the details pane stay in
 * lockstep — see that file's header for the icon/color rules.
 * Anything new that should read consistently across both panels
 * belongs in `badges.ts` first, not here.
 */
import { Fragment, memo, useEffect, useMemo, useRef, type RefObject } from "react";
import { TextAttributes } from "@opentui/core";
import type { ScrollBoxRenderable } from "@opentui/core";

import { type Badge, checkBadge, statusBadge, workStatusBadge } from "../badges.ts";
import {
  recentlyRemovedWorktrees,
  recentRemovalsSummary,
} from "../../core/wtstate.ts";
import {
  BadgeCluster,
  WorktreeBadgeCluster,
  badgeClusterCells,
  modelBadgeSignals,
  worktreeBadgeClusterCells,
  type WorktreeBadgeSignals,
} from "../badge-cluster.tsx";
import {
  rowSpine,
  spineGutterCells,
  StackConnector,
  StatusMarker,
} from "../row-gutter.tsx";
import { scrollCursorIntoView, WtScrollbox } from "../scrollbox.tsx";
import { useScrollToEdge } from "../hooks/useScrollToEdge.ts";
import { NF } from "../icons.ts";
import { Divider } from "./section-divider.tsx";
import { truncateEnd } from "../text.ts";
import { theme } from "../theme.ts";
import type { HarnessId } from "../../core/harness/index.ts";
import type { DerivedState } from "../../core/harness/status.ts";
import type { ReviewRequestPr } from "../../core/github.ts";
import { capitalizeFirst, slugLabel } from "../../core/stage.ts";
import { resolveIssueId } from "../../core/issue-tracker.ts";
import type { SpineCell } from "../../core/stack-layout.ts";
import { StatusKind, type Status } from "../../core/types.ts";
import type { GithubData } from "../../state/queries/github.ts";
import type { ActiveSessionGlyph } from "../hooks/useHarnessSessions.ts";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";
import type { ArchivedItem } from "../hooks/useVisualItems.ts";
import type { WorktreeTarget } from "../../core/worktree-target.ts";
import type { WorktreeModel } from "../worktree-model.ts";
import { remoteWorktreeLedgerKey } from "../../core/worktree-ref.ts";
import {
  isRemoteSummary,
  remoteEntryKey,
  remoteEntryLabel,
  type RemoteListEntry,
} from "../remote-creation.ts";

/**
 * One entry in the ACTIVE portion of the list. Either a worktree row, or a
 * folded section collapsed to a single selectable header line. The parent * (via `tui/hooks/useVisualItems.ts`) builds this so the cursor model and the render share one source
 * of truth — a folded section is one cursor stop, not N hidden rows.
 */
export type FleetWorktreeItem =
  | {
      kind: "wt";
      row: WorktreeRow;
      target: WorktreeTarget;
      model: WorktreeModel;
    }
  | {
      kind: "remote";
      entry: RemoteListEntry;
      target: WorktreeTarget | null;
      model: WorktreeModel | null;
      /** Local fleet-ledger placement; the checkout itself remains remote. */
      archived: boolean;
    };

export type ListActiveItem =
  | FleetWorktreeItem
  | {
      kind: "section";
      /** Section name, or the inbox sentinel. */
      sectionKey: string;
      /** Header label (the section name, or "Inbox"). */
      label: string;
      /** Every collapsed member, local and remote, in expanded-list order. */
      members: FleetWorktreeItem[];
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
  /** Local and remote fleet members in the archived block (never folded). */
  archivedItems: readonly ArchivedItem[];
  /**
   * PRs the user has been asked to review. Pinned in their own section
   * between the active worktrees and the archived block. Not worktrees
   * (no local checkout, no per-slug state) so they render with a
   * stripped-down row component and no badge cluster.
   */
  reviewRequests: readonly ReviewRequestPr[];
  /**
   * Combined cursor index across `items + reviewRequests + archivedItems` in
   * render order. Parent owns the unification so navigation handlers can pick
   * the right item type by index without the list panel re-implementing it.
   */
  selectedIndex: number;
  width: number;
  activeTails: Set<string>;
  /** Slugs with an in-flight tracked headless action. Renders the comment
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
  isLoading: boolean;
  /** The SSH inventory refetch failed; last-known remote rows stay visible. */
  remoteUnavailable: boolean;
  /** One repo-wide GitHub snapshot covering local and remote branches. */
  githubData?: GithubData;
};

/**
 * The same resolved title as details, with its original casing. Layout
 * truncates it to the available cells. Issue IDs keep their numeric prefix
 * in the list; the full ID remains available in details.
 */
export function rowLabel(row: WorktreeRow): string {
  const text = row.title;
  // Stacked rows used to drop the `<id>: ` prefix because a stack
  // section header carried the ID. Stacks render inside the human's
  // sections now and there is no such header, so dropping it just cost
  // those rows their identifier.
  //
  // Resolved, not parsed. This is the row's identifier, so it has to be
  // the same id the issue row links, `i` opens and `{{issue_id}}`
  // renders — reading the slug directly made the list the one surface
  // that ignored the override, so an overridden row was labelled with
  // the ticket it is explicitly NOT, and a row asserted to have no
  // issue still wore one.
  const id = resolveIssueId(row.wt.slug, row.issueId);
  const numId = id ? id.replace(/^[A-Z]+-/, "") : null;
  return numId ? `${numId}: ${text}` : text;
}

/** Display label shared by expanded and folded remote rows. */
export function remoteRowLabel(entry: RemoteListEntry): string {
  const rawLabel = remoteEntryLabel(entry);
  const { id, rest } = slugLabel(rawLabel);
  const numId = id ? id.replace(/^[A-Z]+-/, "") : null;
  return numId ? `${numId}: ${rest || rawLabel}` : rest || rawLabel;
}

/**
 * A split-stack reference names the parent's SECTION, not the parent
 * row. The reference exists so a member whose rail is missing can still
 * be followed, and "somewhere else" is only followable if it says
 * where: the parent's title pointed at a row that is off-screen by
 * definition (that's what makes the stack split) and, when the parent's
 * section is folded, isn't rendered anywhere at all. A section name is
 * a destination — it matches a divider on screen, or a header the user
 * can unfold. Which parent is a details-pane question (the `base` row),
 * and the two never fit together anyway: the list pane is ~50 cells at
 * any terminal width, so a title-plus-section reference would be
 * dropped on every board that has one.
 */
const SECTION_REF_CELLS = 20;
/**
 * Leader for the reference, and a cell held back from the label so the
 * row never lands exactly on its computed budget. `budget` is an
 * arithmetic estimate of a width flexbox actually decides; being one
 * cell optimistic squeezes out the arrow's leading space on precisely
 * the rows whose label fills its allowance, which is most of them.
 */
const REF_ARROW = " → ";
const REF_SLACK = 1;
/** What the inbox is called on screen — divider and reference alike. */
const INBOX_LABEL = "Inbox";
/** Below this the reference is more mystery than signal — drop it. */
const SECTION_REF_MIN = 10;
/**
 * Cells the row's own label must keep before a section reference earns
 * its space. Tuned so a narrow pane drops the reference rather than
 * truncating two siblings of one parent to the same prefix — identity
 * beats relationship when there isn't room for both. The reference
 * shrinks into whatever sits above this line rather than vanishing at a
 * cliff, so a row that gains one badge loses a character instead of the
 * whole pointer.
 */
const MIN_LABEL_CELLS = 24;

const RowView = memo(function RowView({
  row,
  selected,
  isTailing,
  actionRunning,
  activeHarnessId,
  sessionState,
  panelWidth,
  gutterCells,
  spineCell,
  splitParentSection,
}: {
  row: WorktreeRow;
  selected: boolean;
  isTailing: boolean;
  /** Whether a tracked headless action is currently running on this slug. */
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
  gutterCells: number;
  /** This row's rail cell (`rowSpine`), or null for a blank gutter. */
  spineCell: SpineCell | null;
  /** Parent's SECTION when it lives in another one; null otherwise. */
  splitParentSection: string | null;
}) {
  const bg = selected ? theme.rowSelectedBg : undefined;
  // Archived rows render dim (unless selected, where we still want
  // contrast).
  const dimRow = row.archived;
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
  // The row's OWN name wins the space. A section reference is only
  // worth showing if what's left still identifies this row — two
  // siblings of one parent truncated to "Product a..." are exactly the
  // indistinguishable pair the reference was meant to disambiguate.
  const budget = Math.max(
    0,
    panelWidth - 8 - gutterCells - badgeClusterCells(row, actionRunning, activeHarnessId),
  );
  const refRoom = Math.min(
    SECTION_REF_CELLS,
    budget - MIN_LABEL_CELLS - REF_ARROW.length - REF_SLACK,
  );
  const parentRef =
    splitParentSection && refRoom >= SECTION_REF_MIN
      ? `${REF_ARROW}${truncateEnd(splitParentSection, refRoom)}`
      : "";
  const labelCells = budget - Bun.stringWidth(parentRef) - (parentRef ? REF_SLACK : 0);
  return (
    <box
      id={row.wt.slug}
      flexDirection="row"
      backgroundColor={bg}
      paddingLeft={1}
      paddingRight={1}
    >
      <StackConnector row={row} cell={spineCell} cells={gutterCells} />
      <StatusMarker row={row} sessionState={sessionState} />
      <box flexGrow={1} flexShrink={1} overflow="hidden" flexDirection="row">
        {/* Truncation lives in JS, not opentui's native `truncate`,
            because the native path middle-clips with `…`. We want the
            head intact (it's the most distinctive part: "ENG-1234: "
            and the leading words of the title). Width budget = panel
            width − borders(2) − row padding(2) − scrollbar gutter(1) −
            left gutter (3 normal, 4 for a stack row) − badge cluster. */}
        <text fg={slugFg} attributes={slugAttrs} wrapMode="none">
          {truncateEnd(rowLabel(row), labelCells)}
        </text>
        {parentRef ? (
          <box flexShrink={0}>
            <text fg={theme.fgDim} wrapMode="none">{parentRef}</text>
          </box>
        ) : null}
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
  // + scrollbar gutter(1) + leading PR-icon slot(3) + trailing check
  // slot when present(2).
  const trailingCells = showChecks ? 2 + 2 : 0;
  const budget = Math.max(0, panelWidth - 8 - trailingCells);
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
  const count = `[×${String(item.members.length).padStart(2, "0")}]`;
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

/**
 * A checkout owned by an SSH host. It occupies the same section stream as a
 * local worktree; the compact monitor glyph is the only list-level location
 * cue. A transient creation stays visible through install, then the renderer
 * hands off to the summary returned by remote wt.
 */
const RemoteRowView = memo(function RemoteRowView({
  entry,
  selected,
  panelWidth,
  githubData,
  model,
  archived = false,
  unavailable = false,
  actionRunning = false,
}: {
  entry: RemoteListEntry;
  selected: boolean;
  panelWidth: number;
  githubData?: GithubData;
  model: WorktreeModel | null;
  archived?: boolean;
  unavailable?: boolean;
  actionRunning?: boolean;
}) {
  const status: Status = model?.status ?? (isRemoteSummary(entry)
    ? entry.status
    : entry.status === "creating"
      ? { kind: StatusKind.Busy, label: "creating", op: "init" }
      : { kind: StatusKind.Clean, label: "ready" });
  // Remote rows have no badge cluster, so unlike local rows the dirty
  // pencil keeps the marker slot here; only a CLEAN remote row cedes it
  // to the (SSH-carried) work-status dot — which, like local rows,
  // defaults to the dim hollow dot when nothing is asserted.
  const marker =
    status.kind === StatusKind.Clean
      ? workStatusBadge(
          model?.work ?? (isRemoteSummary(entry) ? entry.work : null),
          undefined,
        )
      : statusBadge(status);
  const label = remoteRowLabel(entry);
  const pr = model?.pr ?? (isRemoteSummary(entry) ? githubData?.prs[entry.branch] : undefined);
  const mq = model?.mq ?? (isRemoteSummary(entry)
    ? githubData?.mergeQueue?.[entry.branch]
    : undefined);
  // Two cells for the PUA monitor glyph. It sits immediately before the
  // name so location reads as part of row identity, not as one more
  // right-aligned status badge.
  const remoteCells = 2;
  const badgeSignals: WorktreeBadgeSignals = model
    ? modelBadgeSignals(model, actionRunning)
    : {
        pr,
        mq,
        archived,
        actionRunning,
        environmentLive: false,
        dirty: null,
        rebase: null,
        activeHarnessId: undefined,
        sessionState: undefined,
      };
  const badgeCells = worktreeBadgeClusterCells(badgeSignals);
  return (
    <box
      id={`remote:${remoteEntryKey(entry)}`}
      flexDirection="row"
      paddingLeft={1}
      paddingRight={1}
      backgroundColor={selected ? theme.rowSelectedBg : undefined}
    >
      <box width={2} flexShrink={0}>
        <text fg={archived ? theme.fgDim : marker.fg}>{marker.glyph}</text>
      </box>
      <box width={1} flexShrink={0}>
        <text> </text>
      </box>
      <box width={remoteCells} flexShrink={0}>
        <text fg={archived ? theme.fgDim : unavailable ? theme.warn : theme.info}>
          {NF.remote}
        </text>
      </box>
      <box flexGrow={1} flexShrink={1} overflow="hidden">
        <text
          fg={selected ? theme.fgBright : archived ? theme.fgDim : theme.fg}
          attributes={selected ? TextAttributes.BOLD : 0}
          wrapMode="none"
        >
          {truncateEnd(label, Math.max(0, panelWidth - 8 - remoteCells - badgeCells))}
        </text>
      </box>
      <WorktreeBadgeCluster signals={badgeSignals} />
    </box>
  );
});

/**
 * Memoized: every prop is identity-stable across unrelated App renders
 * (the visual-item arrays come out of `useVisualItems` memos, the row
 * cache keeps `WorktreeRow` references stable, `activeTails` /
 * `activeActions` are identity-stabilized in their hooks), so fetch
 * churn and tail appends elsewhere in the tree skip this whole pane.
 */
export const WorktreeList = memo(function WorktreeList({ items, archivedItems, reviewRequests, selectedIndex, width, activeTails, activeActions, activeSessionBySlug, isLoading, remoteUnavailable, githubData, scrollHandle }: Props) {
  const allRows = [
    ...items.flatMap((i) =>
      i.kind === "wt"
        ? [i.row]
        : i.kind === "section"
          ? i.members.flatMap((member) => member.kind === "wt" ? [member.row] : [])
          : [],
    ),
    ...archivedItems.flatMap((i) => (i.kind === "wt" ? [i.row] : [])),
  ];
  // The rail is laid out per SECTION, over the rows actually drawn and
  // in draw order — a folded section's rows and an archived member are
  // not on screen next to their parent, so they can't be connected to
  // it. Rows inside a folded section get no cell here; the folded
  // summary in the details pane lays out its own.
  const spine = useMemo(() => {
    const bySection = new Map<string, WorktreeRow[]>();
    const push = (bucket: string, row: WorktreeRow) => {
      const g = bySection.get(bucket);
      if (g) g.push(row);
      else bySection.set(bucket, [row]);
    };
    for (const i of items) if (i.kind === "wt") push(`a ${i.row.section ?? ""}`, i.row);
    for (const i of archivedItems) if (i.kind === "wt") push(`z ${i.row.section ?? ""}`, i.row);
    return rowSpine([...bySection.values()]);
  }, [items, archivedItems]);
  // One gutter width for the whole list so the status-dot column is
  // straight across stacked and unstacked rows alike. Zero when nothing
  // visible draws a rail.
  const gutterCells = spineGutterCells(spine);
  // Stack members whose parent was filed in a different section: the
  // rail is replaced by a reference to WHERE the parent went, so the
  // relationship survives the split instead of vanishing (or, worse,
  // pointing at nothing). Inbox parents name the inbox by the same word
  // its divider uses, so the reference always matches something the
  // user can find on screen.
  const splitParentSections = useMemo(() => {
    const byBranch = new Map(allRows.map((r) => [r.wt.branch, r]));
    const m = new Map<string, string>();
    for (const r of allRows) {
      const parentBranch = r.stackedOn?.branch;
      if (!r.stack || !parentBranch) continue;
      const parent = byBranch.get(parentBranch);
      if (parent && parent.section !== r.section) {
        m.set(r.wt.slug, parent.section ?? INBOX_LABEL);
      }
    }
    return m;
  }, [allRows]);
  const hasArchived = archivedItems.length > 0;
  const archivedFolded = archivedItems[0]?.kind === "section";
  const hasReviewRequests = reviewRequests.length > 0;
  const hasActive = items.length > 0;
  // An empty list says WHY it's empty when the removed history shows
  // recent archives — "everything just merged" must not render
  // identically to "nothing was ever created". Gated + memoized so the
  // sync state-file read only happens when the empty state is actually
  // on screen (and once per emptiness transition, not per render).
  const emptyFleet = !hasActive && !hasArchived && !hasReviewRequests && !isLoading;
  const emptySummary = useMemo(
    () =>
      emptyFleet
        ? recentRemovalsSummary(recentlyRemovedWorktrees(new Set()))
        : null,
    [emptyFleet],
  );
  // Index offsets into the combined cursor space owned by the parent
  // (`items + reviewRequests + archivedItems`).
  const reviewOffset = items.length;
  const archivedOffset = reviewOffset + reviewRequests.length;
  // Keep the selected entry scrolled into view, with vim's scrolloff of
  // context beyond it. The whole list (active + review-requests +
  // archived) lives in one scrollbox, so the follow covers every entry;
  // it's a no-op while the row sits comfortably inside the viewport.
  // Child ids: a worktree slug, `section:<key>` for a folded header, or
  // the PR url for review-request rows.
  const listRef = useRef<ScrollBoxRenderable>(null);
  // Expose scroll-to-edge to the parent's j/k handler — reveals trailing
  // blank space / the review + archived headers that sit below the last
  // selectable item.
  useScrollToEdge(listRef, scrollHandle);
  const selItem = selectedIndex < reviewOffset ? items[selectedIndex] : undefined;
  const selectedChildId =
    selItem !== undefined
      ? selItem.kind === "wt"
        ? selItem.row.wt.slug
        : selItem.kind === "remote"
          ? `remote:${remoteEntryKey(selItem.entry)}`
          : `section:${selItem.sectionKey}`
      : selectedIndex < archivedOffset
        ? reviewRequests[selectedIndex - reviewOffset]?.url
        : (() => {
            const archived = archivedItems[selectedIndex - archivedOffset];
            if (!archived) return undefined;
            return archived.kind === "wt"
              ? archived.row.wt.slug
              : archived.kind === "section"
                ? `section:${archived.sectionKey}`
                : `remote:${remoteEntryKey(archived.entry)}`;
          })();
  // Depend on `items`/`reviewRequests`/`archivedItems` (identity-stable per
  // render of the parent) as well as the selected id, so a reflow under a
  // stationary selection — a row inserted above, a section folding/unfolding,
  // an active↔archived split shift — re-runs the follow instead of leaving
  // the cursor drifted off-screen.
  useEffect(() => {
    if (selectedChildId) scrollCursorIntoView(listRef.current, selectedChildId);
  }, [selectedChildId, items, reviewRequests, archivedItems]);
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
        <box padding={1} flexDirection="column">
          {isLoading ? (
            <text fg={theme.fgDim}>Loading worktrees...</text>
          ) : (
            <>
              {/* The summary wraps over several lines on narrow panes, so
                  it gets its own text node — inlining it before the
                  accent `n` sibling makes the flex row render the
                  columns side by side, garbling the message. */}
              {emptySummary ? (
                <text fg={theme.fgDim}>No active worktrees ({emptySummary}).</text>
              ) : null}
              <text fg={theme.fgDim}>
                {emptySummary ? "Press " : "No worktrees. Press "}
                <span fg={theme.accent} attributes={1}>n</span>
                {" to create one."}
              </text>
            </>
          )}
        </box>
      ) : (
        <>
          {!hasActive && !hasArchived ? (
            // No worktrees but review-requests are loaded — still surface
            // the new-worktree hint so the user isn't left wondering where
            // the worktree column went. The PR section renders below.
            <box padding={1}>
              <text fg={theme.fgDim}>
                {"No worktrees. Press "}
                <span fg={theme.accent} attributes={1}>n</span>
                {" to create one."}
              </text>
            </box>
          ) : null}
          {/* The whole list scrolls as one — active worktrees, review
              requests, and the archived block all live in this scrollbox.
              `minHeight={0}` lets it shrink to the flex-allotted height
              instead of growing to fit its content (the default
              `min-height: auto`), which is what makes it actually scroll
              rather than shove the layout. */}
          <WtScrollbox scrollRef={listRef}>
          {items.map((item, i) => {
            // Section context of the previous item (a worktree's section, or a
            // folded section's key) drives the divider/blank-line transitions.
            // `undefined` (no previous item) is distinct from `null` (previous
            // item is in the inbox) so the inbox divider still renders when an
            // inbox row opens the list.
            const prev = i > 0 ? items[i - 1] : undefined;
            const prevSection = prev
              ? prev.kind === "wt"
                ? prev.row.section
                : prev.kind === "remote"
                  ? isRemoteSummary(prev.entry) ? prev.entry.section : null
                  : prev.sectionKey
              : undefined;

            if (item.kind === "remote") {
              const section = isRemoteSummary(item.entry) ? item.entry.section : null;
              return (
                <Fragment key={`active:remote:${remoteEntryKey(item.entry)}`}>
                  {prevSection !== section ? (
                    <>
                      <box height={1} flexShrink={0} />
                      <Divider label={section ?? INBOX_LABEL} width={width} />
                    </>
                  ) : null}
                  <RemoteRowView
                    entry={item.entry}
                    model={item.model}
                    selected={i === selectedIndex}
                    panelWidth={width}
                    githubData={githubData}
                    unavailable={remoteUnavailable}
                    actionRunning={
                      isRemoteSummary(item.entry) &&
                      activeActions.has(remoteWorktreeLedgerKey(item.entry.hostKey, item.entry.slug))
                    }
                  />
                </Fragment>
              );
            }

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
            // between a header and its worktrees. The inbox renders a
            // labeled divider like every other group — even at the very
            // top with nothing else around it — so the list always reads
            // the same regardless of how many groups exist. The leading
            // blank renders above the first header too, so the list opens
            // with breathing room rather than butting it to the border.
            const row = item.row;
            const showDivider = prevSection !== row.section;
            // A row crossing the archive divider changes layout parents.
            // Give each side a distinct reconciliation identity so OpenTUI
            // creates a fresh render node instead of trying to reparent the
            // archived node (which can leave it detached and invisible).
            return (
              <Fragment key={`active:local:${row.wt.slug}`}>
                {showDivider ? (
                  <>
                    <box height={1} flexShrink={0} />
                    <Divider
                      label={row.section ?? INBOX_LABEL}
                      width={width}
                    />
                  </>
                ) : null}
                <RowView
                  gutterCells={gutterCells}
                  spineCell={spine.get(row.wt.slug) ?? null}
                  splitParentSection={splitParentSections.get(row.wt.slug) ?? null}
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
              {archivedFolded ? null : <Divider label="Archived" width={width} />}
              {archivedItems.map((item, i) => {
                const globalIndex = archivedOffset + i;
                // Folded, the block IS its header — the `[×NN]` chip
                // replaces the divider, exactly as a folded section
                // does up in the active list.
                if (item.kind === "section") {
                  return (
                    <FoldedSectionHeader
                      key={`section:${item.sectionKey}`}
                      item={item}
                      selected={globalIndex === selectedIndex}
                    />
                  );
                }
                if (item.kind === "remote") {
                  return (
                    <RemoteRowView
                      key={`archived:remote:${remoteEntryKey(item.entry)}`}
                      entry={item.entry}
                      model={item.model}
                      selected={globalIndex === selectedIndex}
                      panelWidth={width}
                      githubData={githubData}
                      archived
                      unavailable={remoteUnavailable}
                      actionRunning={activeActions.has(
                        remoteWorktreeLedgerKey(item.entry.hostKey, item.entry.slug),
                      )}
                    />
                  );
                }
                const row = item.row;
                return (
                  <RowView
                    gutterCells={gutterCells}
                    spineCell={spine.get(row.wt.slug) ?? null}
                    splitParentSection={splitParentSections.get(row.wt.slug) ?? null}
                    key={`archived:local:${row.wt.slug}`}
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
          </WtScrollbox>
        </>
      )}
    </box>
  );
});
