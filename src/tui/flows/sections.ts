/**
 * Section-management flows: moving rows/groups, the section picker, and
 * section rename. Extracted from `app.tsx`; rebuilt per render (like
 * `flows/destroy.ts`) so the closures see fresh rows / selection /
 * wtstate. Pure imperative logic — no hooks.
 */
import type { Dispatch, SetStateAction } from "react";

import type { WtState } from "../../core/wtstate.ts";
import type { FooterMode } from "../panels/footer.tsx";
import type { SectionPickerItem } from "../panels/section-picker.tsx";
import type { Modal } from "../modal-state.ts";
import {
  GROUP_INBOX,
  STACK_SECTION_PREFIX,
  type WorktreeRow,
} from "../hooks/useWorktreeRows.ts";
import { theme } from "../theme.ts";

type SectionFlowsCtx = {
  rows: WorktreeRow[];
  current: WorktreeRow | undefined;
  selectedSection: { sectionKey: string; isStack: boolean } | undefined;
  wtState: WtState | undefined;
  lastMoveTarget: string | null;
  setLastMoveTarget: (v: string | null) => void;
  setModal: Dispatch<SetStateAction<Modal | null>>;
  setFooter: (f: FooterMode) => void;
  setPendingRename: (v: string | null) => void;
  toast: (message: string, color?: string, ms?: number) => void;
  reportActionError: (label: string, err: unknown) => void;
  setSection: (slug: string, section: string | null) => Promise<void>;
  placeSlug: (
    slug: string,
    section: string | null,
    position: "top" | "bottom",
  ) => Promise<void>;
  swapOrder: (
    slugA: string,
    slugB: string,
    section: string | null,
    bucketDisplay: readonly string[],
  ) => Promise<void>;
  moveGroupPast: (
    key: string,
    pastKey: string,
    side: "before" | "after",
    visualOrder: readonly string[],
  ) => Promise<boolean>;
};

export function makeSectionFlows(ctx: SectionFlowsCtx) {
  const {
    rows,
    current,
    selectedSection,
    wtState,
    lastMoveTarget,
    setLastMoveTarget,
    setModal,
    setFooter,
    setPendingRename,
    toast,
    reportActionError,
    setSection,
    placeSlug,
    swapOrder,
    moveGroupPast,
  } = ctx;

  /**
   * Build the section-picker item list. Excludes the current row's
   * section since "move this row to where it already is" is never
   * useful (the user explicitly asked to drop that as clutter).
   * "+ new section" sits at the bottom with `l` as its quick chord
   * trigger so `l l` creates a fresh section in two keystrokes.
   */
  function buildSectionItems(currentRow: WorktreeRow): SectionPickerItem[] {
    const items: SectionPickerItem[] = [];
    const currentSection = currentRow.section;
    if (currentSection !== null) items.push({ kind: "none" });
    const seen = new Set<string>();
    for (const r of rows) {
      if (r.archived) continue;
      if (r.section === null || seen.has(r.section)) continue;
      seen.add(r.section);
      if (r.section === currentSection) continue;
      // Inferred stack sections aren't manually joinable — skip
      // them so the picker only lists manual named sections.
      if (r.sectionIsStack) continue;
      items.push({ kind: "section", name: r.section });
    }
    items.push({ kind: "create" });
    return items;
  }

  /**
   * Move a whole group (a stack section, a manual section, the inbox)
   * one display slot in `dir`. The landmark is the adjacent group that
   * currently RENDERS rows — `moveGroupPast` then jumps any invisible
   * group sitting in between (an empty inbox) so one keypress is one
   * visual step, never a phantom no-change move.
   */
  function doMoveGroup(groupKey: string, dir: -1 | 1, what: string): void {
    // Drive the move off the ON-SCREEN group order (distinct group keys
    // in display-sorted row order), not `sectionsOrder`. An inferred
    // stack section isn't in `sectionsOrder` until its first move, so a
    // `sectionsOrder`-derived sequence would never find it (idx = -1) and
    // silently no-op — the exact reason stack sections couldn't be moved.
    // `moveGroupPast` seeds the stack key at its visual slot using the
    // `vseq` we pass, so the reorder is coherent with what the user sees.
    const vseq: string[] = [];
    const seen = new Set<string>();
    for (const r of rows) {
      if (r.archived) continue;
      const g = r.section ?? GROUP_INBOX;
      if (!seen.has(g)) {
        seen.add(g);
        vseq.push(g);
      }
    }
    const idx = vseq.indexOf(groupKey);
    if (idx < 0) return; // group mid-refresh; self-heals on next read
    const neighbor = vseq[idx + dir];
    if (!neighbor) {
      toast(
        dir > 0 ? `${what} already at bottom` : `${what} already at top`,
        theme.fgDim,
        1500,
      );
      return;
    }
    moveGroupPast(groupKey, neighbor, dir > 0 ? "after" : "before", vseq).then(
      (moved) => {
        if (moved) toast(`moved ${what} ${dir > 0 ? "down" : "up"}`, theme.info, 1200);
      },
      (err) => reportActionError("move", err),
    );
  }

  /**
   * Unified Shift+J/K — moves the smallest movable thing under the
   * cursor:
   *   - A row in the inbox / a manual section: swap with its same-group
   *     neighbor, or slide to the near edge of the adjacent group across
   *     a boundary (top of next on `J`, bottom of prev on `K`). Stack
   *     sections can't be joined (membership is inferred from the
   *     fork-base records), so a sliding row hops over them in one
   *     keypress; the inbox is a valid target even when empty.
   *   - A row inside a stack section: rows there are ordered by the
   *     base-record topology, so the move applies to the WHOLE stack —
   *     one group slot.
   *   - A folded section header (stack or manual): the whole group moves.
   * The archive boundary is hard: rows can't cross into archived via
   * J/K — that's `a`'s job.
   */
  function doShiftMove(dir: -1 | 1): void {
    if (selectedSection) {
      doMoveGroup(
        selectedSection.sectionKey,
        dir,
        selectedSection.isStack ? "stack" : "section",
      );
      return;
    }
    if (!current) return;
    if (current.archived) {
      toast("archived rows don't reorder, use `a` to restore", theme.fgDim, 1500);
      return;
    }
    if (current.sectionIsStack) {
      doMoveGroup(current.section!, dir, "stack");
      return;
    }
    const active = rows.filter((r) => !r.archived);
    const idx = active.indexOf(current);
    if (idx < 0) return;
    const slug = current.wt.slug;
    const target = active[idx + dir];
    if (target && target.section === current.section) {
      const bucket = active
        .filter((r) => r.section === current.section)
        .map((r) => r.wt.slug);
      swapOrder(slug, target.wt.slug, current.section, bucket).catch((err) =>
        reportActionError("reorder", err),
      );
      return;
    }
    // Crossing a group boundary: land at the near edge of the adjacent
    // group in the ranked sequence. Built from `sectionsOrder` rather
    // than the neighboring ROW so stack sections get skipped and the
    // inbox is reachable even when it has no rows (the only way back
    // out when every row is sectioned).
    const order = wtState?.sectionsOrder ?? [];
    const present = new Set<string>();
    for (const r of active) present.add(r.section ?? GROUP_INBOX);
    const seq = order.filter((g) => g === GROUP_INBOX || present.has(g));
    const start = seq.indexOf(current.section ?? GROUP_INBOX);
    if (start < 0) return; // unranked mid-refresh; self-heals on next read
    let i = start + dir;
    while (i >= 0 && i < seq.length && seq[i]!.startsWith(STACK_SECTION_PREFIX)) {
      i += dir;
    }
    const targetGroup = seq[i];
    if (targetGroup === undefined) {
      toast(dir > 0 ? "already at bottom" : "already at top", theme.fgDim, 1500);
      return;
    }
    const sectionVal = targetGroup === GROUP_INBOX ? null : targetGroup;
    placeSlug(slug, sectionVal, dir > 0 ? "top" : "bottom").then(
      () => toast(`moved to ${sectionVal ?? "Inbox"}`, theme.info, 1200),
      (err) => reportActionError("move", err),
    );
  }

  function openSectionPicker(): void {
    if (!current) return;
    if (current.archived) {
      toast("archived rows don't have a section context, use `a` to restore", theme.fgDim, 2000);
      return;
    }
    if (current.sectionIsStack) {
      toast("stack rows are ordered by their base records — move the whole stack", theme.fgDim, 1800);
      return;
    }
    const items = buildSectionItems(current);
    // Default cursor: sticky last-move-target if it's still in the
    // list (and isn't the current section), else the first item.
    // The user's most common workflow is "move several rows into the
    // same section", and forcing them to re-aim every time eats keys.
    let initial = 0;
    if (lastMoveTarget !== null && lastMoveTarget !== current.section) {
      const i = items.findIndex(
        (it) => it.kind === "section" && it.name === lastMoveTarget,
      );
      if (i >= 0) initial = i;
    }
    setModal({
      kind: "sectionPicker",
      title: `move ${current.wt.slug} to section`,
      slug: current.wt.slug,
      items,
      index: initial,
      newName: null,
    });
  }

  function commitSectionPick(item: SectionPickerItem, slug: string): void {
    if (item.kind === "none") {
      setSection(slug, null).then(
        () => toast("moved to Inbox", theme.info, 1500),
        (err) => reportActionError("move", err),
      );
      setLastMoveTarget(null);
      setModal(null);
      return;
    }
    if (item.kind === "section") {
      const target = item.name;
      setSection(slug, target).then(
        () => toast(`moved to ${target}`, theme.info, 1500),
        (err) => reportActionError("move", err),
      );
      setLastMoveTarget(target);
      setModal(null);
      return;
    }
    // "+ new section" — switch to input mode. Submission lives in the
    // keyboard handler.
    setModal((m) =>
      m?.kind === "sectionPicker" ? { ...m, newName: "" } : m,
    );
  }

  /**
   * Open the rename prompt for the current row's section. No-op for
   * unsectioned and archived rows — there's no nameable section to
   * rename in those contexts.
   */
  function openSectionRename(): void {
    if (!current || current.archived) return;
    if (current.section === null) {
      toast("the Inbox can't be renamed", theme.fgDim, 1500);
      return;
    }
    if (current.sectionIsStack) {
      toast("stack section name is auto-derived", theme.fgDim, 1500);
      return;
    }
    setPendingRename(current.section);
    setFooter({
      kind: "input",
      prompt: `rename "${current.section}":`,
      value: current.section,
      purpose: "rename-section",
    });
  }

  return { doShiftMove, openSectionPicker, commitSectionPick, openSectionRename };
}
