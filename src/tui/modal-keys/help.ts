import type { KeyEvent } from "@opentui/core";

import { printableText } from "../app-helpers.ts";
import { handleOverlayScrollKey } from "../scrollbox.tsx";
import type { Modal } from "../modal-state.ts";
import { applyEditKey, emptyEdit, insertText } from "../text-edit.tsx";
import type { SimpleModalContext } from "./ctx.ts";

export function handleHelpKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "help" }>,
  { setModal }: Pick<SimpleModalContext, "setModal">,
): boolean {
  if (modal.searching) {
    if (k.ctrl && k.name === "c") {
      setModal(null);
      return true;
    }
    if (k.name === "escape") {
      setModal({ ...modal, searching: false, query: emptyEdit });
      return true;
    }
    if (k.name === "return") {
      setModal({ ...modal, searching: false });
      return true;
    }
    // Backspace on empty query leaves search mode.
    if (k.name === "backspace" && modal.query.value.length === 0) {
      setModal({ ...modal, searching: false });
      return true;
    }
    // Cursor movement / deletion — shared editor logic.
    const edited = applyEditKey(k, modal.query);
    if (edited) {
      setModal({ ...modal, query: edited });
      return true;
    }
    const text = printableText(k.sequence);
    if (text) setModal({ ...modal, query: insertText(modal.query, text) });
    return true;
  }
  // Shared overlay scroll keymap (j/k, PgUp/PgDn, g/G, Home/End) —
  // only outside search mode, where j/g/etc. are literal query text.
  if (handleOverlayScrollKey(k)) return true;
  if (k.sequence === "/") {
    setModal({ ...modal, searching: true });
    return true;
  }
  if (k.name === "escape" && modal.query.value) {
    setModal({ ...modal, query: emptyEdit });
    return true;
  }
  if (
    k.name === "escape" ||
    k.sequence === "?" ||
    k.name === "q" ||
    (k.ctrl && k.name === "c")
  ) {
    setModal(null);
  }
  return true;
}
