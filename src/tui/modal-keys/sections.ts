import type { KeyEvent } from "@opentui/core";

import { isPlainLetter, printableText } from "../app-helpers.ts";
import type { Modal } from "../modal.ts";
import type { SimpleModalContext } from "./ctx.ts";

export function handleSectionPickerKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "sectionPicker" }>,
  ctx: SimpleModalContext,
): boolean {
  const {
    setModal,
    consumePrTargetChord,
    setSection,
    setLastMoveTarget,
    toast,
    reportActionError,
    commitSectionPick,
    infoColor,
  } = ctx;
  if (consumePrTargetChord(k)) {
    setModal(null);
    return true;
  }
  if (modal.newName !== null) {
    if (k.name === "escape") {
      setModal({ ...modal, newName: null });
      return true;
    }
    if (k.ctrl && k.name === "c") {
      setModal(null);
      return true;
    }
    if (k.name === "return") {
      const name = modal.newName.trim();
      if (!name) {
        setModal({ ...modal, newName: null });
        return true;
      }
      const slug = modal.slug;
      setSection(slug, name).then(
        () => toast(`moved to ${name}`, infoColor, 1500),
        (err) => reportActionError("move", err),
      );
      setLastMoveTarget(name);
      setModal(null);
      return true;
    }
    if (k.name === "backspace") {
      if (modal.newName.length === 0) {
        setModal({ ...modal, newName: null });
        return true;
      }
      setModal({ ...modal, newName: modal.newName.slice(0, -1) });
      return true;
    }
    const text = printableText(k.sequence);
    if (text) setModal({ ...modal, newName: modal.newName + text });
    return true;
  }
  if (k.name === "j" || k.name === "down") {
    setModal({ ...modal, index: Math.min(modal.index + 1, modal.items.length - 1) });
    return true;
  }
  if (k.name === "k" || k.name === "up") {
    setModal({ ...modal, index: Math.max(modal.index - 1, 0) });
    return true;
  }
  if (isPlainLetter(k, "n")) {
    const createIdx = modal.items.findIndex((it) => it.kind === "create");
    if (createIdx >= 0) commitSectionPick(modal.items[createIdx]!, modal.slug);
    return true;
  }
  if (k.sequence && /^[1-9]$/.test(k.sequence)) {
    const i = parseInt(k.sequence, 10) - 1;
    const item = modal.items[i];
    if (item && item.kind !== "create") commitSectionPick(item, modal.slug);
    return true;
  }
  if (k.name === "return" || isPlainLetter(k, "l")) {
    const item = modal.items[modal.index];
    if (item) commitSectionPick(item, modal.slug);
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
