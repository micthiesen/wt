import type { KeyEvent } from "@opentui/core";

import { isPlainLetter } from "../app-helpers.ts";
import type { Modal } from "../modal.ts";
import { previewFocusPatch } from "../picker-preview.ts";
import type { SimpleModalContext } from "./ctx.ts";

export function handleBranchPickerKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "branchPicker" }>,
  { setModal }: SimpleModalContext,
): boolean {
  if (k.name === "j" || k.name === "down") {
    setModal({ ...modal, index: Math.min(modal.index + 1, modal.items.length - 1) });
    return true;
  }
  if (k.name === "k" || k.name === "up") {
    setModal({ ...modal, index: Math.max(modal.index - 1, 0) });
    return true;
  }
  if (k.name === "return") {
    const chosen = modal.items[modal.index]!;
    modal.resolve(chosen);
    setModal(null);
    return true;
  }
  if (
    k.name === "escape" ||
    k.sequence === "q" ||
    (k.ctrl && k.name === "c")
  ) {
    modal.resolve(null);
    setModal(null);
  }
  return true;
}

export function handleBasePickerKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "basePicker" }>,
  { setModal, commitBasePick }: SimpleModalContext,
): boolean {
  if (k.name === "j" || k.name === "down") {
    setModal({ ...modal, index: Math.min(modal.index + 1, modal.items.length - 1) });
    return true;
  }
  if (k.name === "k" || k.name === "up") {
    setModal({ ...modal, index: Math.max(modal.index - 1, 0) });
    return true;
  }
  if (k.sequence && /^[1-9]$/.test(k.sequence)) {
    const item = modal.items[parseInt(k.sequence, 10) - 1];
    if (item) commitBasePick(item, modal.slug);
    return true;
  }
  if (k.name === "return" || isPlainLetter(k, "b")) {
    const item = modal.items[modal.index];
    if (item) commitBasePick(item, modal.slug);
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

export function handleOutputsPickerKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "outputsPicker" }>,
  { setModal, visibleOutputs, currentSlug, setFocus }: SimpleModalContext,
): boolean {
  const idx =
    visibleOutputs.length === 0
      ? 0
      : Math.min(Math.max(0, modal.index), visibleOutputs.length - 1);
  const moveTo = (next: number): void => {
    setModal({ kind: "outputsPicker", index: next });
    const patch = previewFocusPatch(visibleOutputs[next]?.id ?? null);
    if (patch) setFocus(currentSlug ?? null, patch);
  };
  const commit = (i: number): void => {
    const target = visibleOutputs[i];
    if (target) setFocus(currentSlug ?? null, { focused: target.id });
    setModal(null);
  };
  if (k.name === "j" || k.name === "down") {
    moveTo(Math.min(idx + 1, visibleOutputs.length - 1));
    return true;
  }
  if (k.name === "k" || k.name === "up") {
    moveTo(Math.max(0, idx - 1));
    return true;
  }
  if (k.sequence && /^[1-9]$/.test(k.sequence)) {
    const i = parseInt(k.sequence, 10) - 1;
    if (visibleOutputs[i]) commit(i);
    return true;
  }
  if (k.sequence === "'" || k.name === "return") {
    commit(idx);
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
