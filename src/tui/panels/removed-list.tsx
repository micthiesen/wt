/**
 * Removed-worktrees view for the left pane (`h` toggles it in and out).
 * Renders the persisted removed history (`WtState.removed`) instead of
 * live worktrees: no per-slug sources exist anymore, so rows are a
 * snapshotted work-status glyph + label + PR glyph + age.
 * Entries whose slug is live again are filtered out by the parent.
 */
import { memo, useEffect, useMemo, useRef } from "react";
import type React from "react";
import { TextAttributes } from "@opentui/core";
import type { ScrollBoxRenderable } from "@opentui/core";

import type { RemovedWorktree } from "../../core/wtstate.ts";
import { capitalizeFirst, slugLabel } from "../../core/stage.ts";
import { workStatusBadge } from "../badges.ts";
import { dayBucket, dayLabel } from "../day-headers.ts";
import { NF } from "../icons.ts";
import { scrollCursorIntoView, WtScrollbox } from "../scrollbox.tsx";
import { ageMsToText, truncateEnd } from "../text.ts";
import { theme } from "../theme.ts";

/** The last asserted work status, or unknown for entries without a snapshot. */
export function removedStatusGlyph(entry: RemovedWorktree): { glyph: string; fg: string } {
  if (!entry.work) return { glyph: "?", fg: theme.fgDim };
  return workStatusBadge(entry.work, undefined, false, entry.prState === "MERGED");
}

/** PR-state glyph for a removed row; a dim trash glyph when no PR was recorded. */
export function removedPrGlyph(entry: RemovedWorktree): { glyph: string; fg: string } {
  switch (entry.prState) {
    case "MERGED":
      return { glyph: NF.prMerged, fg: theme.ok };
    case "CLOSED":
      return { glyph: NF.prClosed, fg: theme.err };
    case "OPEN":
      return { glyph: NF.prOpen, fg: theme.accentAlt };
    default:
      return { glyph: NF.trash, fg: theme.fgDim };
  }
}

/**
 * Row label, matching the live list's shape: numeric issue id prefix +
 * the snapshotted title, falling back to the slug's descriptive tail.
 */
export function removedRowLabel(entry: RemovedWorktree): string {
  const { id, rest } = slugLabel(entry.slug);
  const text = capitalizeFirst(entry.title ?? (rest || entry.slug));
  const numId = id ? id.replace(/^[A-Z]+-/, "") : null;
  return numId ? `${numId}: ${text}` : text;
}

/** Compact right-aligned age cell ("3d", "2h"); empty for unparsable dates. */
function removedAge(entry: RemovedWorktree): string {
  const t = Date.parse(entry.removedAt);
  return Number.isFinite(t) ? ageMsToText(Date.now() - t) : "";
}

const RemovedRowView = memo(function RemovedRowView({
  entry,
  selected,
  panelWidth,
}: {
  entry: RemovedWorktree;
  selected: boolean;
  panelWidth: number;
}) {
  const status = removedStatusGlyph(entry);
  const pr = removedPrGlyph(entry);
  const age = removedAge(entry);
  const fg = selected ? theme.fgBright : theme.fgDim;
  const attrs = selected ? TextAttributes.BOLD : 0;
  // Width budget mirrors the live rows: borders(2) + padding(2) +
  // scrollbar gutter(1) + leading status slot(3) + trailing PR
  // slot(3) and age cell when present.
  const trailingCells = 3 + (age.length > 0 ? age.length + 2 : 0);
  const budget = Math.max(0, panelWidth - 8 - trailingCells);
  return (
    <box
      id={`removed:${entry.slug}`}
      flexDirection="row"
      backgroundColor={selected ? theme.rowSelectedBg : undefined}
      paddingLeft={1}
      paddingRight={1}
    >
      <box flexShrink={0} flexDirection="row">
        <box width={2} flexShrink={0}>
          <text fg={status.fg}>{status.glyph}</text>
        </box>
        <box width={1} flexShrink={0}>
          <text> </text>
        </box>
      </box>
      <box flexGrow={1} flexShrink={1} overflow="hidden">
        <text fg={fg} attributes={attrs} wrapMode="none">
          {truncateEnd(removedRowLabel(entry), budget)}
        </text>
      </box>
      <box width={2} flexShrink={0}>
        <text fg={pr.fg}>{pr.glyph}</text>
      </box>
      <box width={1} flexShrink={0}>
        <text> </text>
      </box>
      {age.length > 0 ? (
        <box flexShrink={0} flexDirection="row">
          <text>  </text>
          <text fg={theme.fgDim}>{age}</text>
        </box>
      ) : null}
    </box>
  );
});

/** A dim day header ("today", "yesterday", "Sat 15 Aug") above its group. */
const DayHeader = memo(function DayHeader({
  text,
  spaced,
}: {
  text: string;
  spaced: boolean;
}) {
  return (
    <box
      flexDirection="row"
      paddingLeft={1}
      paddingRight={1}
      marginTop={spaced ? 1 : 0}
    >
      <text fg={theme.fgDim} attributes={TextAttributes.BOLD}>
        {text}
      </text>
    </box>
  );
});

export function RemovedList({
  entries,
  selectedIndex,
  width,
}: {
  entries: readonly RemovedWorktree[];
  selectedIndex: number;
  width: number;
}) {
  const listRef = useRef<ScrollBoxRenderable>(null);
  // Interleave a header wherever the day-bucket changes. `entries` is
  // stored newest-first (`recordRemovedWorktrees` sorts on write), so
  // comparing against the previous entry is enough — no regrouping, and
  // the rows keep their original indices, which is what `selectedIndex`
  // and the cursor-scroll child id are addressed by.
  //
  // One `now` for the whole pass so every "today"/"yesterday" in a
  // single render agrees, even if the clock crosses 04:00 mid-render.
  const rendered = useMemo(() => {
    const now = Date.now();
    const out: React.ReactNode[] = [];
    let prev: string | null = null;
    entries.forEach((entry, i) => {
      const bucket = dayBucket(entry.removedAt);
      // A null bucket (unparsable `removedAt`) gets no header rather
      // than a fabricated day, matching the age cell's behaviour. It
      // still resets `prev`, so the next real day re-announces itself.
      if (bucket !== null && bucket !== prev) {
        out.push(
          <DayHeader
            key={`day:${bucket}`}
            text={dayLabel(bucket, now)}
            // The scrollbox already opens with a blank line, so the
            // first header must not add a second one.
            spaced={out.length > 0}
          />,
        );
      }
      prev = bucket;
      out.push(
        <RemovedRowView
          key={entry.slug}
          entry={entry}
          selected={i === selectedIndex}
          panelWidth={width}
        />,
      );
    });
    return out;
  }, [entries, selectedIndex, width]);
  const selectedChildId = entries[selectedIndex]
    ? `removed:${entries[selectedIndex]!.slug}`
    : undefined;
  useEffect(() => {
    if (selectedChildId) scrollCursorIntoView(listRef.current, selectedChildId);
  }, [selectedChildId, entries]);
  return (
    <box
      flexDirection="column"
      width={width}
      flexShrink={0}
      border
      borderStyle="single"
      borderColor={theme.border}
      title={` removed (${entries.length}) `}
      titleAlignment="left"
      paddingTop={0}
    >
      {entries.length === 0 ? (
        <box padding={1} flexDirection="row">
          <text fg={theme.fgDim}>No removed worktrees. Press </text>
          <text fg={theme.accent} attributes={1}>
            h
          </text>
          <text fg={theme.fgDim}> to go back.</text>
        </box>
      ) : (
        <WtScrollbox scrollRef={listRef}>
          <box height={1} flexShrink={0} />
          {rendered}
        </WtScrollbox>
      )}
    </box>
  );
}
