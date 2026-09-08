import type { ActionVars } from "../core/actions.ts";
import type { Modal } from "./modal-state.ts";
import { ActionPickerModal, ActionEditModal, type PickerItem } from "./panels/action-picker.tsx";
import { ArgPickerModal } from "./panels/picker.tsx";

export type ActionPopupModal = Extract<Modal, { kind: "actionPicker" | "argPicker" }>;
export function isActionPopupModal(modal: Modal | null): modal is ActionPopupModal {
  return modal?.kind === "actionPicker" ? modal.state.surface === "row"
    : modal?.kind === "argPicker" && modal.target !== undefined;
}
export type ActionPopupSnapshot = { modal: ActionPopupModal; items: PickerItem[]; vars: ActionVars };

/** Same views as the normal modal host; execution stays in the explorer. */
export function ActionPopupView({ modal, items, vars }: ActionPopupSnapshot) {
  if (modal.kind === "argPicker") return <ArgPickerModal
    title={modal.def.name} prompt={modal.def.argPrompt?.label ?? ""}
    history={modal.history} index={Math.min(Math.max(0, modal.index), modal.history.length)} input={modal.input} />;
  const state = modal.state;
  return state.mode === "list"
    ? <ActionPickerModal slug={state.slug} surface={state.surface} items={items} selectedIndex={state.index} />
    : <ActionEditModal slug={state.slug} surface={state.surface} def={state.def} extras={state.extras} vars={vars} />;
}
