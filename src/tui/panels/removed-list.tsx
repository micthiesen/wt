/**
 * Removed-worktrees view for the left pane (`h` toggles it in and out).
 * Renders the persisted removed history (`WtState.removed`) instead of
 * live worktrees: no per-slug sources exist anymore, so rows are a
 * stripped-down glyph + label + age, mirroring the review-request rows.
 * Entries whose slug is live again are filtered out by the parent.
 */
import { memo, useEffect, useRef } from "react";
import { TextAttributes } from "@opentui/core";
import type { ScrollBoxRenderable } from "@opentui/core";

import type { RemovedWorktree } from "../../core/wtstate.ts";
import { capitalizeFirst, slugLabel } from "../../core/stage.ts";
import { NF } from "../icons.ts";
import { useScrollbarNoFlash } from "../hooks/useScrollbarNoFlash.ts";
import { ageMsToText, truncateEnd } from "../text.ts";
import { theme } from "../theme.ts";

/** PR-state glyph for a removed row; a dim trash glyph when no PR was recorded. */
export function removedGlyph(entry: RemovedWorktree): { glyph: string; fg: string } {
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
  const marker = removedGlyph(entry);
  const age = removedAge(entry);
  const fg = selected ? theme.fgBright : theme.fgDim;
  const attrs = selected ? TextAttributes.BOLD : 0;
  // Width budget mirrors the live rows: borders(2) + padding(2) +
  // leading glyph slot(3) + trailing age cell when present.
  const trailingCells = age.length > 0 ? age.length + 2 : 0;
  const budget = Math.max(0, panelWidth - 7 - trailingCells);
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
          <text fg={marker.fg}>{marker.glyph}</text>
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
      {age.length > 0 ? (
        <box flexShrink={0} flexDirection="row">
          <text>  </text>
          <text fg={theme.fgDim}>{age}</text>
        </box>
      ) : null}
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
  const listScrollRef = useScrollbarNoFlash(listRef);
  const selectedChildId = entries[selectedIndex]
    ? `removed:${entries[selectedIndex]!.slug}`
    : undefined;
  useEffect(() => {
    if (selectedChildId) listRef.current?.scrollChildIntoView(selectedChildId);
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
        <scrollbox ref={listScrollRef} scrollY flexGrow={1} minHeight={0}>
          <box height={1} flexShrink={0} />
          {entries.map((entry, i) => (
            <RemovedRowView
              key={entry.slug}
              entry={entry}
              selected={i === selectedIndex}
              panelWidth={width}
            />
          ))}
        </scrollbox>
      )}
    </box>
  );
}
