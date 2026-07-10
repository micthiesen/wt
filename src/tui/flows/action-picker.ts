/**
 * Action-picker helpers (`!`): build the grouped picker item list,
 * availability gating, and the open helper. Extracted from `app.tsx`;
 * rebuilt per render so the closures see fresh rows.
 */
import {
  BUILTIN_ACTIONS,
  evaluateActionRequirements,
} from "../../core/actions.ts";
import { config } from "../../core/config.ts";
import type { Modal } from "../modal-state.ts";
import { assignActionKeys, type PickerItem } from "../panels/action-picker.tsx";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";
import { theme } from "../theme.ts";

type ActionPickerFlowsCtx = {
  rows: WorktreeRow[];
  setModal: (m: Modal | null) => void;
  toast: (message: string, color?: string, ms?: number) => void;
};

export function makeActionPickerFlows(ctx: ActionPickerFlowsCtx) {
  const { rows, setModal, toast } = ctx;

  function buildActionPickerItems(slug: string): PickerItem[] {
    const row = rows.find((r) => r.wt.slug === slug);
    const rowState = {
      pr: row?.pr,
      deployed: row?.fields.deploy.data ?? false,
    };
    const defs = [...config.actions, ...BUILTIN_ACTIONS];
    const keyById = assignActionKeys(defs);
    const actionItems = defs.map((def) => ({
      kind: "action" as const,
      def,
      key: keyById.get(def.id) ?? "",
      availability: evaluateActionRequirements(def.requires, rowState),
    }));
    // Cluster by group: group order by first appearance, original order
    // within a group, so the picker shows one header per section. Keys
    // are assigned over the unclustered list above so they stay stable
    // regardless of grouping. The custom-prompt entry always trails.
    const buckets = new Map<string, typeof actionItems>();
    for (const it of actionItems) {
      const g = it.def.group ?? "";
      const arr = buckets.get(g);
      if (arr) arr.push(it);
      else buckets.set(g, [it]);
    }
    return [...[...buckets.values()].flat(), { kind: "custom" as const }];
  }

  /**
   * Returns true if the item is launchable. For unavailable actions
   * toasts the reason so the user understands the no-op without
   * having to scan the dim subtitle in the picker. Used at both the
   * Enter and quick-pick-digit handlers so an unavailable action
   * can't slip into the edit modal.
   */
  function canPickAction(item: PickerItem): boolean {
    if (item.kind === "custom") return true;
    if (item.availability.ok) return true;
    toast(`${item.def.name}: ${item.availability.reason}`, theme.warn, 2500);
    return false;
  }

  function openActionPicker(slug: string): void {
    setModal({
      kind: "actionPicker",
      state: { mode: "list", slug, index: 0 },
    });
  }

  return { buildActionPickerItems, canPickAction, openActionPicker };
}
