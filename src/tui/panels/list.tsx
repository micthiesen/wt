/**
 * Worktree row list (left pane).
 *
 * Status, PR-state, check, and merge-queue glyphs all come from
 * `tui/badges.ts` so this panel and the details pane stay in
 * lockstep — see that file's header for the icon/color rules.
 * Anything new that should read consistently across both panels
 * belongs in `badges.ts` first, not here.
 */
import { memo } from "react";
import { TextAttributes } from "@opentui/core";

import { prStateBadge, statusBadge } from "../badges.ts";
import { NF } from "../icons.ts";
import { Spinner } from "../spinner.tsx";
import { ELLIPSIS, ELLIPSIS_WIDTH } from "../text.ts";
import { theme } from "../theme.ts";
import { capitalizeFirst, slugLabel } from "../../core/stage.ts";
import { type MergeQueueState, StatusKind } from "../../core/types.ts";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";

type Props = {
  rows: WorktreeRow[];
  selectedIndex: number;
  width: number;
  activeTails: Set<string>;
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
 * "<mq-glyph>N" for a merge-queue position: nerd-font merge-queue
 * octicon + 1-based position (`+` if there are ≥10 ahead).
 */
function mqGlyph(row: WorktreeRow): string {
  const mq = row.mq;
  // Empty placeholder fills the 4-cell slot (2-cell icon + 1-cell space
  // + 1-cell digit). The space prevents the icon's right-half from
  // overlapping the digit when opentui's native renderer treats the
  // icon as 1-cell wide — the explicit gap is sturdier than relying
  // on the renderer's wide-char handling.
  if (!mq) return "    ";
  const pos = mq.position;
  const digit = pos >= 10 ? "+" : String(pos);
  return `${NF.mergeQueue} ${digit}`;
}

/**
 * Row label text. `row.title` is the resolved title (always non-empty,
 * `llm > pr > commit > slug` fallback owned by `useWorktreeRows`).
 * First char is capitalized to match PR-title convention even when an
 * LLM emits lowercase; harmless on already-capitalized slug fallbacks.
 * Issue ID, when present, prefixes the label as `ENG-1234: <text>`.
 */
function rowLabel(row: WorktreeRow): string {
  const { id } = slugLabel(row.wt.slug);
  const text = capitalizeFirst(row.title);
  return id ? `${id}: ${text}` : text;
}

/**
 * End-truncate `s` to fit within `maxWidth` terminal cells, suffixing
 * `...` when it overflows. OpenTUI's native `truncate` flag does
 * middle-truncation in the binary with the same 3-cell ASCII ellipsis;
 * we want trailing ellipsis (label starts the same way for related
 * branches, so the head is the most recognizable part), so we shorten
 * ourselves and skip the native flag — but match its glyph for visual
 * consistency.
 */
function truncateEnd(s: string, maxWidth: number): string {
  if (maxWidth <= 0) return "";
  if (Bun.stringWidth(s) <= maxWidth) return s;
  if (maxWidth < ELLIPSIS_WIDTH) return ELLIPSIS.slice(0, maxWidth);
  let cut = s;
  while (cut.length > 0 && Bun.stringWidth(cut) + ELLIPSIS_WIDTH > maxWidth) {
    cut = cut.slice(0, -1);
  }
  return `${cut}${ELLIPSIS}`;
}

/**
 * Cells the badge cluster occupies for a given row. Mirrors the
 * width-prop layout in the JSX below: 2-cell leading gap + each present
 * badge's box width. Returns 0 when no badges are rendered so the slug
 * column reclaims the space. The refresh hint, when present, sits as
 * the leftmost slot inside the cluster.
 */
function badgeClusterCells(row: WorktreeRow): number {
  const isDeployed = row.fields.deploy.data ?? false;
  const showChecks =
    !!row.pr && row.pr.state === "OPEN" && row.pr.checks !== "none";
  const refreshing = isRefreshing(row);
  const hasAnyBadge = refreshing || !!(row.pr || row.mq || isDeployed);
  if (!hasAnyBadge) return 0;
  let cells = 2; // leading gap
  if (refreshing) cells += 2;
  if (row.pr) cells += 2;
  if (showChecks) cells += 2;
  if (row.mq) cells += 4;
  if (isDeployed) cells += 2;
  return cells;
}

/**
 * Tiny CI rollup glyph rendered next to the PR badge. Only shown for
 * live PRs — after merge/close the check state is noise.
 */
function checkGlyph(row: WorktreeRow): { glyph: string; fg: string } {
  const pr = row.pr;
  if (!pr || pr.state !== "OPEN") return { glyph: "  ", fg: theme.fgDim };
  switch (pr.checks) {
    case "pass":
      return { glyph: NF.checkPass, fg: theme.ok };
    case "fail":
      return { glyph: NF.checkFail, fg: theme.err };
    case "pending":
      return { glyph: NF.checkPend, fg: theme.warn };
    default:
      return { glyph: "  ", fg: theme.fgDim };
  }
}

const RowView = memo(function RowView({
  row,
  selected,
  isTailing,
  panelWidth,
}: {
  row: WorktreeRow;
  selected: boolean;
  isTailing: boolean;
  panelWidth: number;
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
  const prb = row.pr
    ? prStateBadge(row.pr)
    : { glyph: "  ", fg: theme.fgDim };
  const prFg = row.archived ? theme.fgDim : prb.fg;
  const c = checkGlyph(row);
  const checkFg = row.archived ? theme.fgDim : c.fg;
  const deployFg = row.archived
    ? theme.fgDim
    : (row.fields.deploy.data ?? false)
      ? theme.warn
      : theme.fgDim;
  const mqFg = row.archived || !row.mq ? theme.fgDim : mqColor(row.mq.state);
  const mqText = mqGlyph(row);
  const isDeployed = row.fields.deploy.data ?? false;
  const showChecks =
    row.pr && row.pr.state === "OPEN" && row.pr.checks !== "none";
  const refreshing = isRefreshing(row);
  const hasAnyBadge = refreshing || !!(row.pr || row.mq || isDeployed);
  // OpenTUI `attributes` is a bitmask over TextAttributes. Combine BOLD
  // (selection) and ITALIC (tailing) so both indicators survive when
  // a row is both selected and being tailed.
  const slugAttrs =
    (selected ? TextAttributes.BOLD : 0) |
    (isTailing ? TextAttributes.ITALIC : 0);
  return (
    <box
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
          {truncateEnd(rowLabel(row), Math.max(0, panelWidth - 7 - badgeClusterCells(row)))}
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
          {refreshing ? (
            <box width={2} flexShrink={0}>
              <Spinner fg={theme.fgDim} />
            </box>
          ) : null}
          {row.pr ? (
            <box width={2} flexShrink={0}>
              <text fg={prFg}>{prb.glyph}</text>
            </box>
          ) : null}
          {showChecks ? (
            <box width={2} flexShrink={0}>
              <text fg={checkFg}>{c.glyph}</text>
            </box>
          ) : null}
          {row.mq ? (
            <box width={4} flexShrink={0}>
              <text fg={mqFg}>{mqText}</text>
            </box>
          ) : null}
          {isDeployed ? (
            <box width={2} flexShrink={0}>
              <text fg={deployFg}>{NF.bolt}</text>
            </box>
          ) : null}
        </box>
      ) : null}
    </box>
  );
});

function Divider({ label, width }: { label: string; width: number }) {
  // Leave room for padding (border+paddingLeft+paddingRight roughly 4
  // cells) so the rule doesn't bleed past the panel edge.
  const inner = Math.max(0, width - 4);
  const labelStr = ` ${label} `;
  const padding = Math.max(0, inner - labelStr.length - 2);
  const trail = "─".repeat(padding);
  return (
    <box flexDirection="row" paddingLeft={1} paddingRight={1}>
      <text fg={theme.borderDim}>──</text>
      <text fg={theme.fgDim}>{labelStr}</text>
      <text fg={theme.borderDim}>{trail}</text>
    </box>
  );
}

export function WorktreeList({ rows, selectedIndex, width, activeTails, isLoading, filter }: Props) {
  const firstArchivedIndex = rows.findIndex((r) => r.archived);
  const hasArchived = firstArchivedIndex !== -1;
  const activeRows = hasArchived ? rows.slice(0, firstArchivedIndex) : rows;
  const archivedRows = hasArchived ? rows.slice(firstArchivedIndex) : [];
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
      {rows.length === 0 ? (
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
          {activeRows.map((row, i) => (
            <RowView
              key={row.wt.slug}
              row={row}
              selected={i === selectedIndex}
              isTailing={activeTails.has(row.wt.slug)}
              panelWidth={width}
            />
          ))}
          {hasArchived ? (
            <>
              {/* Pushes the archived section to the bottom of the pane. */}
              <box flexGrow={1} flexShrink={1} />
              <Divider label="archived" width={width} />
              {archivedRows.map((row, i) => {
                const globalIndex = activeRows.length + i;
                return (
                  <RowView
                    key={row.wt.slug}
                    row={row}
                    selected={globalIndex === selectedIndex}
                    isTailing={activeTails.has(row.wt.slug)}

                    panelWidth={width}
                  />
                );
              })}
            </>
          ) : null}
        </>
      )}
    </box>
  );
}
