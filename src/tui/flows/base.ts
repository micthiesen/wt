/**
 * Fork-base picker flow (`b`): record / clear a worktree's base — the
 * record stacks are inferred from, so this is also how a worktree is
 * stacked on (or unstacked from) a sibling by hand. Extracted from
 * `app.tsx`; rebuilt per render so the closures see fresh rows /
 * selection.
 */
import { config } from "../../core/config.ts";
import type { Worktree } from "../../core/types.ts";
import type { Modal } from "../modal-state.ts";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";
import { theme } from "../theme.ts";

type BaseFlowsCtx = {
  rows: WorktreeRow[];
  current: WorktreeRow | undefined;
  setModal: (m: Modal | null) => void;
  toast: (message: string, color?: string, ms?: number) => void;
  reportActionError: (label: string, err: unknown) => void;
  setBase: (wt: Worktree, branch: string | null) => Promise<void>;
};

export function makeBaseFlows(ctx: BaseFlowsCtx) {
  const { rows, current, setModal, toast, reportActionError, setBase } = ctx;

  function openBasePicker(): void {
    if (!current) return;
    if (current.archived) {
      toast("archived rows have no live worktree to diff", theme.fgDim, 2000);
      return;
    }
    const recorded = current.stackedOn?.branch ?? null;
    // A branch already based on THIS worktree (directly or transitively)
    // is excluded: picking a descendant as the base would close a record
    // cycle, which the layout degrades to flat rows — a silent trap.
    const parentOf = new Map(
      rows
        .filter((r) => !r.archived && r.stackedOn)
        .map((r) => [r.wt.branch, r.stackedOn!.branch] as const),
    );
    const basedOnCurrent = (branch: string): boolean => {
      const seen = new Set<string>();
      let b: string | undefined = branch;
      while (b && !seen.has(b)) {
        if (b === current.wt.branch) return true;
        seen.add(b);
        b = parentOf.get(b);
      }
      return false;
    };
    const siblings = rows
      .filter(
        (r) =>
          !r.archived &&
          r.wt.slug !== current.wt.slug &&
          !basedOnCurrent(r.wt.branch),
      )
      .map((r) => r.wt.branch);
    // A recorded base whose worktree was already cleaned (branch kept)
    // wouldn't show up via the rows scan — surface it anyway so the
    // "(current)" marker is always visible.
    if (recorded && !siblings.includes(recorded)) siblings.unshift(recorded);
    const items = [
      {
        label: `none — diff against ${config.branch.base}`,
        branch: null as string | null,
      },
      ...siblings.map((b) => ({
        label: b === recorded ? `${b} (current)` : b,
        branch: b as string | null,
      })),
    ];
    const idx = recorded ? items.findIndex((it) => it.branch === recorded) : 0;
    setModal({
      kind: "basePicker",
      slug: current.wt.slug,
      items,
      index: Math.max(0, idx),
    });
  }

  function commitBasePick(
    item: { label: string; branch: string | null },
    slug: string,
  ): void {
    setModal(null);
    const row = rows.find((r) => r.wt.slug === slug);
    if (!row) return;
    setBase(row.wt, item.branch).then(
      () =>
        toast(
          item.branch
            ? `base → ${item.branch} (record only, no rebase)`
            : `base cleared — diffing against ${config.branch.base}`,
          theme.info,
          2000,
        ),
      (err) => reportActionError("set base", err),
    );
  }

  return { openBasePicker, commitBasePick };
}
