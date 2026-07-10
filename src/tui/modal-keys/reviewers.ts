import type { KeyEvent } from "@opentui/core";

import type { Modal } from "../modal-state.ts";
import type { SimpleModalContext } from "./ctx.ts";

export function handleReviewerPickerKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "reviewerPicker" }>,
  { setModal, submitReviewerPicker }: SimpleModalContext,
): boolean {
  if (k.name === "j" || k.name === "down") {
    setModal({ ...modal, index: Math.min(modal.index + 1, modal.items.length - 1) });
    return true;
  }
  if (k.name === "k" || k.name === "up") {
    setModal({ ...modal, index: Math.max(modal.index - 1, 0) });
    return true;
  }
  if (k.name === "space" || k.sequence === " ") {
    const item = modal.items[modal.index];
    if (item) {
      const next = new Set(modal.checked);
      if (next.has(item.key)) next.delete(item.key);
      else next.add(item.key);
      setModal({ ...modal, checked: next });
    }
    return true;
  }
  if (k.name === "return" || k.sequence === "v") {
    void submitReviewerPicker();
    return true;
  }
  if (
    k.name === "escape" ||
    k.sequence === "q" ||
    (k.ctrl && k.name === "c")
  ) {
    setModal(null);
  }
  return true;
}
