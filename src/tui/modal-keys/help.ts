import type { KeyEvent } from "@opentui/core";

import { printableText } from "../app-helpers.ts";
import type { Modal } from "../modal-state.ts";
import type { SimpleModalContext } from "./ctx.ts";

export function handleHelpKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "help" }>,
  { setModal }: SimpleModalContext,
): boolean {
  if (modal.searching) {
    if (k.ctrl && k.name === "c") {
      setModal(null);
      return true;
    }
    if (k.name === "escape") {
      setModal({ ...modal, searching: false, query: "" });
      return true;
    }
    if (k.name === "return") {
      setModal({ ...modal, searching: false });
      return true;
    }
    if (k.name === "backspace") {
      if (modal.query.length === 0) {
        setModal({ ...modal, searching: false });
        return true;
      }
      setModal({ ...modal, query: modal.query.slice(0, -1) });
      return true;
    }
    const text = printableText(k.sequence);
    if (text) setModal({ ...modal, query: modal.query + text });
    return true;
  }
  if (k.sequence === "/") {
    setModal({ ...modal, searching: true });
    return true;
  }
  if (k.name === "escape" && modal.query) {
    setModal({ ...modal, query: "" });
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
