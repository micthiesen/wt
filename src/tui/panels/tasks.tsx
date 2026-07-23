/**
 * Task-inbox row list — the hub mode's central pane. Renders the flat,
 * pre-sorted `TaskItem[]` from `useTaskRows` as a two-line-per-task list
 * with section dividers between buckets. Purely presentational: every
 * piece of state (bucket, reason, detail, badges) arrives already
 * derived on the item, this file only lays it out.
 *
 * Glyphs and badge machinery are shared with the classic worktree list
 * (`panels/list.tsx`) wherever the same concept applies — `rowLabel`,
 * `BadgeCluster`/`badgeClusterCells`, the harness glyph/tint — so a
 * worktree reads identically whether you're looking at it from the
 * classic list or the task inbox. The task glyph (line 1's leading
 * icon) is new here: it encodes the task BUCKET, not the worktree
 * status, since that's the axis the inbox is organized around.
 */
import { Fragment, memo, useEffect, useRef, type RefObject } from "react";
import { TextAttributes } from "@opentui/core";
import type { ScrollBoxRenderable } from "@opentui/core";

import { BadgeCluster, badgeClusterCells } from "../badge-cluster.tsx";
import { useScrollbarNoFlash } from "../hooks/useScrollbarNoFlash.ts";
import { useScrollToEdge } from "../hooks/useScrollToEdge.ts";
import { truncateEnd } from "../text.ts";
import { theme } from "../theme.ts";
import { NF } from "../icons.ts";
import { getHarness, type HarnessId } from "../../core/harness/index.ts";
import { stateColor } from "../claude-state.ts";
import { TASK_BUCKET_LABEL, type TaskBucket } from "../../core/task-state.ts";
import { capitalizeFirst } from "../../core/stage.ts";
import { stackOrdinalLabel } from "../../core/stack-layout.ts";
import type { ActiveSessionGlyph } from "../hooks/useHarnessSessions.ts";
import type { DisplayBucket, TaskItem } from "../hooks/useTaskRows.ts";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";
import { rowLabel } from "./list.tsx";
import { Divider } from "./section-divider.tsx";

/** Imperative scroll-to-edge control, same contract as `ListScrollHandle` in `panels/list.tsx`. */
export type TaskListHandle = { toEdge: (dir: "top" | "bottom") => void };

type Props = {
  tasks: readonly TaskItem[];
  selectedIndex: number;
  width: number;
  isLoading: boolean;
  activeSessionBySlug: ReadonlyMap<string, ActiveSessionGlyph>;
  activeActions: ReadonlySet<string>;
  /**
   * Whether this tmux pane holds keyboard focus. Tints the panel border
   * (accent = typing lands here) — the in-pane half of the hub's focus
   * signal; the harness pane is deliberately left unmarked.
   */
  paneFocused?: boolean;
  /** TAB-cycled primary harness — the glyph a dead Sessions slot shows. */
  primaryHarness: HarnessId;
  scrollHandle?: RefObject<TaskListHandle | null>;
};

/** Fixed left-gutter width every task row reserves for its state glyph: 2-cell glyph + 1-cell gap. */
const GUTTER_CELLS = 3;
/** Line-2 detail indent: same width as the gutter, so the reason text lines up under the title, not the glyph. */
const DETAIL_INDENT = GUTTER_CELLS;
/** borders(2) + row padding(2) + gutter(3) — the fixed overhead subtracted from `width` for both lines' truncation budgets. */
const FIXED_OVERHEAD = 2 + 2 + GUTTER_CELLS;

type Glyph = { glyph: string; fg: string };

/**
 * Line-1 leading glyph for a task, by kind:
 *
 *  - `pr` items show the PR-open/draft glyph (mirrors
 *    `ReviewRequestRowView` in `panels/list.tsx`) instead of a bucket
 *    glyph — the review-request section is inherently "needs-you" or
 *    "waiting", so the PR state itself is the more useful icon.
 *  - `wt`/`stack` items show a bucket glyph. `working` is special: when
 *    the row has a live session, reuse that session's own tinted
 *    harness glyph (exactly what the classic list's badge cluster
 *    shows) rather than a generic spinner, so hub mode doesn't
 *    invent a second "is it working" indicator that can disagree with
 *    the one everywhere else.
 */
function taskGlyph(
  item: TaskItem,
  activeSessionBySlug: ReadonlyMap<string, ActiveSessionGlyph>,
  primaryHarness: HarnessId,
): Glyph {
  if (item.kind === "pr") {
    return item.pr.isDraft
      ? { glyph: NF.prDraft, fg: theme.fgDim }
      : { glyph: NF.prOpen, fg: theme.accentAlt };
  }
  if (item.kind === "slot") {
    // Same robot the footer's slot glyphs used: live sessions tint by
    // state, a dead slot shows the primary harness's glyph dimmed.
    const active = activeSessionBySlug.get(item.slot.slug);
    if (active) {
      const fg = active.state
        ? stateColor(active.harnessId, active.state)
        : getHarness(active.harnessId).color;
      return { glyph: getHarness(active.harnessId).glyph, fg };
    }
    return { glyph: getHarness(primaryHarness).glyph, fg: theme.fgDim };
  }
  return bucketGlyph(item.state.bucket, item.row, activeSessionBySlug);
}

function bucketGlyph(
  bucket: TaskBucket,
  row: WorktreeRow,
  activeSessionBySlug: ReadonlyMap<string, ActiveSessionGlyph>,
): Glyph {
  switch (bucket) {
    case "needs-you":
      return { glyph: "!", fg: theme.err };
    case "review-output":
      return { glyph: "●", fg: theme.warn };
    case "ready":
      return { glyph: "✓", fg: theme.ok };
    case "working": {
      const active = activeSessionBySlug.get(row.wt.slug);
      if (active) {
        const fg = active.state
          ? stateColor(active.harnessId, active.state)
          : getHarness(active.harnessId).color;
        return { glyph: getHarness(active.harnessId).glyph, fg };
      }
      return { glyph: "…", fg: theme.info };
    }
    case "waiting":
      return { glyph: "◌", fg: theme.fgDim };
    case "idle":
      return { glyph: "·", fg: theme.fgDim };
    case "done":
      return { glyph: "✔", fg: theme.fgDim };
  }
}

/**
 * Line-1 title text, before the pin prefix: `rowLabel` for a worktree
 * (with a spine ordinal prefixed when it's an expanded stack member,
 * simplified from the classic list's full `StackGutter` connector+
 * ordinal to just the ordinal — the inbox isn't drawing a tree), the
 * stack's resolved label when collapsed, or the PR title.
 */
function taskTitle(item: TaskItem): string {
  if (item.kind === "slot") return item.slot.label;
  if (item.kind === "pr") return capitalizeFirst(item.pr.title);
  if (item.kind === "stack") return item.label;
  const label = rowLabel(item.row);
  if (item.inExpandedStack && item.row.stack) {
    return `${stackOrdinalLabel(item.row.stack.ordinal)} ${label}`;
  }
  return label;
}

/** Line-2 text: `state.reason` + optional detail + optional "(snoozed)" suffix. */
function taskDetailLine(
  item: TaskItem,
  activeSessionBySlug: ReadonlyMap<string, ActiveSessionGlyph>,
): string {
  if (item.kind === "slot") {
    const state = activeSessionBySlug.get(item.slot.slug)?.state;
    const status = state ?? "no session";
    return item.collapsedGroup ? `${status} · tab: more` : status;
  }
  const base = item.detail ? `${item.state.reason} — ${item.detail}` : item.state.reason;
  return item.state.snoozed ? `${base} (snoozed)` : base;
}

const TaskRowView = memo(function TaskRowView({
  item,
  selected,
  activeSessionBySlug,
  activeActions,
  primaryHarness,
  panelWidth,
}: {
  item: TaskItem;
  selected: boolean;
  activeSessionBySlug: ReadonlyMap<string, ActiveSessionGlyph>;
  activeActions: ReadonlySet<string>;
  primaryHarness: HarnessId;
  panelWidth: number;
}) {
  const bg = selected ? theme.rowSelectedBg : undefined;
  const titleFg = selected ? theme.fgBright : theme.fg;
  const attrs = selected ? TextAttributes.BOLD : 0;
  const glyph = taskGlyph(item, activeSessionBySlug, primaryHarness);

  const isRowKind = item.kind === "wt" || item.kind === "stack";
  const row = isRowKind ? item.row : undefined;
  const hasBadges = isRowKind;
  const actionRunning = row ? activeActions.has(row.wt.slug) : false;
  const activeHarnessId = row ? activeSessionBySlug.get(row.wt.slug)?.harnessId : undefined;
  const sessionState = row ? (activeSessionBySlug.get(row.wt.slug)?.state ?? undefined) : undefined;
  const badgeCells = row ? badgeClusterCells(row, actionRunning, activeHarnessId) : 0;

  const pinned = isRowKind && item.manual.pinned;
  const rawTitle = taskTitle(item);
  const titleText = pinned ? `${NF.pin} ${rawTitle}` : rawTitle;
  const budget1 = Math.max(0, panelWidth - FIXED_OVERHEAD - badgeCells);
  const budget2 = Math.max(0, panelWidth - FIXED_OVERHEAD);

  return (
    <box id={item.key} flexDirection="column" backgroundColor={bg} paddingLeft={1} paddingRight={1}>
      <box flexDirection="row">
        <box width={2} flexShrink={0}>
          <text fg={glyph.fg} attributes={attrs}>{glyph.glyph}</text>
        </box>
        <box width={1} flexShrink={0}>
          <text> </text>
        </box>
        <box flexGrow={1} flexShrink={1} overflow="hidden">
          <text fg={titleFg} attributes={attrs} wrapMode="none">
            {truncateEnd(titleText, budget1)}
          </text>
        </box>
        {hasBadges && row ? (
          <BadgeCluster
            row={row}
            actionRunning={actionRunning}
            activeHarnessId={activeHarnessId}
            sessionState={sessionState}
          />
        ) : null}
      </box>
      <box flexDirection="row">
        <box width={DETAIL_INDENT} flexShrink={0} />
        <box flexGrow={1} flexShrink={1} overflow="hidden">
          <text fg={theme.fgDim} wrapMode="none">
            {truncateEnd(taskDetailLine(item, activeSessionBySlug), budget2)}
          </text>
        </box>
      </box>
    </box>
  );
});

/** Divider label for a `displayBucket` value — the two synthetic buckets get their own copy, everything else is a real `TaskBucket` read from `TASK_BUCKET_LABEL`. */
function dividerLabel(displayBucket: DisplayBucket): string {
  if (displayBucket === "pinned") return "Pinned";
  if (displayBucket === "snoozed") return "Snoozed";
  if (displayBucket === "sessions") return "Sessions";
  // Excluding the three literals above narrows `displayBucket` from
  // `DisplayBucket` straight to `TaskBucket` — no `as` cast needed.
  return TASK_BUCKET_LABEL[displayBucket];
}

export function TaskList({
  tasks,
  selectedIndex,
  width,
  isLoading,
  activeSessionBySlug,
  activeActions,
  paneFocused = false,
  primaryHarness,
  scrollHandle,
}: Props) {
  const hasWork = tasks.some((t) => t.kind !== "slot");
  const listRef = useRef<ScrollBoxRenderable>(null);
  const listScrollRef = useScrollbarNoFlash(listRef);
  useScrollToEdge(listRef, scrollHandle);

  const selectedId = tasks[selectedIndex]?.key;
  useEffect(() => {
    if (selectedId) listRef.current?.scrollChildIntoView(selectedId);
  }, [selectedId, tasks]);

  return (
    <box
      flexDirection="column"
      width={width}
      flexGrow={1}
      flexShrink={1}
      minHeight={0}
      border
      borderStyle="single"
      borderColor={paneFocused ? theme.accent : theme.borderDim}
      title=" tasks "
      titleAlignment="left"
      paddingTop={0}
    >
      {!hasWork ? (
        // The Sessions slot group is always present (see `useTaskRows`),
        // so `tasks` itself is never empty — `hasWork` (any non-`slot`
        // item) is the real "is there anything worth showing" signal.
        // The hint renders ABOVE the still-rendered Sessions group below
        // rather than replacing the whole pane, so the loading/empty
        // state and the Sessions group are never mutually exclusive.
        <box padding={1} flexDirection="row">
          {isLoading ? (
            <text fg={theme.fgDim}>Loading tasks...</text>
          ) : (
            <>
              <text fg={theme.fgDim}>No tasks. Press </text>
              <text fg={theme.accent} attributes={1}>
                n
              </text>
              <text fg={theme.fgDim}> to create a worktree.</text>
            </>
          )}
        </box>
      ) : null}
      <scrollbox ref={listScrollRef} scrollY flexGrow={1} minHeight={0}>
        {tasks.map((item, i) => {
          const prev = i > 0 ? tasks[i - 1] : undefined;
          const showDivider = prev === undefined || prev.displayBucket !== item.displayBucket;
          return (
            <Fragment key={item.key}>
              {showDivider ? (
                <>
                  <box height={1} flexShrink={0} />
                  <Divider label={dividerLabel(item.displayBucket)} width={width} />
                </>
              ) : null}
              <TaskRowView
                item={item}
                selected={i === selectedIndex}
                activeSessionBySlug={activeSessionBySlug}
                activeActions={activeActions}
                primaryHarness={primaryHarness}
                panelWidth={width}
              />
            </Fragment>
          );
        })}
      </scrollbox>
    </box>
  );
}
