/**
 * Removed-worktrees history view state (`h` toggles the left pane into
 * it). The cursor is a plain index over the filtered entries — the list
 * is static-ish and has no sections/folding, so the key-based cursor
 * model of the live list would be overkill here.
 */
import { useMemo, useState } from "react";
import { useQuery } from "@tanstack/react-query";

import { config } from "../../core/config.ts";
import { issueStatusIds } from "../../core/issue-status.ts";
import type { RemovedWorktree, WtState } from "../../core/wtstate.ts";
import { issueStatusesQuery } from "../../state/queries/issue-status.ts";
import { productionCommitsQuery, watchedBranchTipsQuery } from "../../state/queries/worktree.ts";
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
  // The history list and selected details share this one batch observer.
  // Do not poll archived issues while the ordinary fleet view is showing.
  const issueIds = useMemo(
    () => issueStatusIds(removedEntries, config.issueTracker?.prefix),
    [removedEntries],
  );
  const issueStatuses = useQuery({
    ...issueStatusesQuery(issueIds),
    enabled: removedView && issueIds.length > 0 && config.issueTracker?.statusCommand != null,
  });
  const production = config.branch.production;
  const promotionCommits = useMemo(() => [
    ...new Set(removedEntries
      .filter((e) => e.landedOnAtRemoval === "base" && e.prMergeCommitOid)
      .map((e) => e.prMergeCommitOid!)),
  ], [removedEntries]);
  const productionTip = useQuery({
    ...watchedBranchTipsQuery(production ? [production] : []),
    enabled: removedView && !!production && production !== config.branch.base && promotionCommits.length > 0,
  });
  const productionCommits = useQuery({
    ...productionCommitsQuery(
      production,
      production ? productionTip.data?.[production] : undefined,
      promotionCommits,
    ),
    enabled: removedView && !!production && production !== config.branch.base &&
      !!productionTip.data?.[production] && promotionCommits.length > 0,
  });
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
    removedIssueStatuses: issueStatuses.data,
    removedProductionCommits: productionCommits.data,
  };
}
