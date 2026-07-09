/**
 * Removed-worktrees history view state (`h` toggles the left pane into
 * it). The cursor is a plain index over the filtered entries — the list
 * is static-ish and has no sections/folding, so the key-based cursor
 * model of the live list would be overkill here.
 */
import { useMemo, useState } from "react";

import type { RemovedWorktree, WtState } from "../../core/wtstate.ts";
import type { WorktreeRow } from "./useWorktreeRows.ts";

export function useRemovedView(opts: {
  rows: WorktreeRow[];
  wtState: WtState | undefined;
}) {
  const { rows, wtState } = opts;
  const [removedView, setRemovedView] = useState(false);
  const [removedIndex, setRemovedIndex] = useState(0);
  // Hide entries whose slug is live again: a failed destroy leaves the
  // worktree in place (the record self-heals into view only if it ever
  // actually disappears), and a restored slug drops out immediately even
  // before `createWorktree` clears its record.
  const removedEntries = useMemo(() => {
    const live = new Set(rows.map((r) => r.wt.slug));
    return (wtState?.removed ?? []).filter((e) => !live.has(e.slug));
  }, [rows, wtState?.removed]);
  const removedCursor = Math.min(
    removedIndex,
    Math.max(0, removedEntries.length - 1),
  );
  const currentRemoved: RemovedWorktree | undefined = removedView
    ? removedEntries[removedCursor]
    : undefined;

  return {
    removedView,
    setRemovedView,
    setRemovedIndex,
    removedEntries,
    removedCursor,
    currentRemoved,
  };
}
