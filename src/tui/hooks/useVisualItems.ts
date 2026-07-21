import { useMemo, useRef } from "react";
import { useQuery } from "@tanstack/react-query";

import type { ReviewRequestPr } from "../../core/github.ts";
import { reviewRequestsQuery } from "../../state/index.ts";
import type { ListActiveItem } from "../panels/list.tsx";
import type { RemoteCreation } from "../remote-creation.ts";
import type { RemoteWorktreeSummary } from "../../core/remote-worktrees.ts";
import { remoteEntryKey } from "../remote-creation.ts";
import {
  GROUP_INBOX,
  type WorktreeRow,
} from "./useWorktreeRows.ts";

export type VisualItem = ListActiveItem | { kind: "pr"; pr: ReviewRequestPr };
export type SelectedSection = Extract<ListActiveItem, { kind: "section" }>;

export function visualKey(item: VisualItem): string {
  return item.kind === "wt"
    ? item.row.wt.slug
    : item.kind === "remote"
      ? `remote:${remoteEntryKey(item.entry)}`
    : item.kind === "section"
      ? `section:${item.sectionKey}`
      : `pr:${item.pr.url}`;
}

type UseVisualItemsArgs = {
  rows: readonly WorktreeRow[];
  foldedSections: ReadonlySet<string>;
  stackSectionLabels: ReadonlyMap<string, string>;
  selectedKey: string | null;
  remoteCreation: RemoteCreation | null;
  remoteWorktrees: readonly RemoteWorktreeSummary[];
};

export function useVisualItems({
  rows,
  foldedSections,
  stackSectionLabels,
  selectedKey,
  remoteCreation,
  remoteWorktrees,
}: UseVisualItemsArgs) {
  // When the selected slug disappears, this ref snaps the cursor to the
  // row that took its place rather than jumping to the top of the list.
  const lastIndexRef = useRef(0);

  const reviewRequests = useQuery(reviewRequestsQuery());
  const reviewRequestRows = useMemo<readonly ReviewRequestPr[]>(
    () => reviewRequests.data ?? [],
    [reviewRequests.data],
  );

  const archivedRows = useMemo(() => rows.filter((r) => r.archived), [rows]);

  // Active portion, with folded sections collapsed to one `section` item each.
  // This is the single source of truth shared by the cursor model and the list.
  const activeItems = useMemo<ListActiveItem[]>(() => {
    const activeRows = rows.filter((r) => !r.archived);
    const out: ListActiveItem[] = [];
    for (const entry of remoteWorktrees) out.push({ kind: "remote", entry });
    if (
      remoteCreation &&
      !remoteWorktrees.some((row) => row.slug === remoteCreation.input)
    ) {
      out.push({ kind: "remote", entry: remoteCreation });
    }
    const emitted = new Set<string>();
    for (const r of activeRows) {
      const sec = r.section ?? GROUP_INBOX;
      if (foldedSections.has(sec)) {
        if (emitted.has(sec)) continue;
        emitted.add(sec);
        out.push({
          kind: "section",
          sectionKey: sec,
          isStack: r.sectionIsStack,
          label:
            r.section === null
              ? "Inbox"
              : r.sectionIsStack
                ? stackSectionLabels.get(sec) ?? sec
                : sec,
          rows: activeRows.filter((x) => (x.section ?? GROUP_INBOX) === sec),
        });
      } else {
        out.push({ kind: "wt", row: r });
      }
    }
    return out;
  }, [rows, foldedSections, stackSectionLabels, remoteCreation, remoteWorktrees]);

  const visualItems = useMemo<VisualItem[]>(() => {
    const prs: VisualItem[] = reviewRequestRows.map((pr) => ({ kind: "pr", pr }));
    const archived: VisualItem[] = archivedRows.map((r) => ({ kind: "wt", row: r }));
    return [...activeItems, ...prs, ...archived];
  }, [activeItems, reviewRequestRows, archivedRows]);

  // Resolve the selected key to a visual index. When the key isn't in
  // the current visible set, fall back to the last known visual index,
  // clamped to the new length.
  const lookupIndex =
    selectedKey === null
      ? -1
      : visualItems.findIndex((v) => visualKey(v) === selectedKey);
  const cursorIndex = (() => {
    if (visualItems.length === 0) return -1;
    if (lookupIndex >= 0) return lookupIndex;
    if (selectedKey === null) {
      const firstWt = visualItems.findIndex(
        (v) => v.kind === "wt" || v.kind === "remote",
      );
      return firstWt >= 0 ? firstWt : -1;
    }
    return Math.min(lastIndexRef.current, visualItems.length - 1);
  })();

  const currentItem = cursorIndex >= 0 ? visualItems[cursorIndex] : undefined;
  const current = currentItem?.kind === "wt" ? currentItem.row : undefined;
  const selectedPr = currentItem?.kind === "pr" ? currentItem.pr : undefined;
  const selectedRemote =
    currentItem?.kind === "remote" ? currentItem.entry : undefined;
  const selectedSection =
    currentItem?.kind === "section" ? currentItem : undefined;

  // Render-time write is derived from this render's inputs and mirrors the
  // previous in-app cursor model.
  if (cursorIndex >= 0 && cursorIndex !== lastIndexRef.current) {
    lastIndexRef.current = cursorIndex;
  }

  return {
    activeItems,
    archivedRows,
    reviewRequestRows,
    visualItems,
    cursorIndex,
    currentItem,
    current,
    selectedPr,
    selectedRemote,
    selectedSection,
  };
}
