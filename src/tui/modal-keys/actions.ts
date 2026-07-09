import type { KeyEvent } from "@opentui/core";

import { recentValues } from "../../core/action-history.ts";
import { printableMultiline } from "../app-helpers.ts";
import type { Modal } from "../modal.ts";
import type { SimpleModalContext } from "./ctx.ts";

export function handleActionPickerKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "actionPicker" }>,
  ctx: SimpleModalContext,
): boolean {
  const {
    setModal,
    rows,
    buildActionPickerItems,
    canPickAction,
    launchAction,
    toast,
    warnColor,
  } = ctx;
  const ap = modal.state;
  if (ap.mode === "list") {
    const items = buildActionPickerItems(ap.slug);
    if (k.name === "j" || k.name === "down") {
      setModal({
        kind: "actionPicker",
        state: { ...ap, index: Math.min(ap.index + 1, items.length - 1) },
      });
      return true;
    }
    if (k.name === "k" || k.name === "up") {
      setModal({
        kind: "actionPicker",
        state: { ...ap, index: Math.max(ap.index - 1, 0) },
      });
      return true;
    }
    const commitIndex = (i: number): void => {
      const item = items[i];
      if (!item) return;
      if (!canPickAction(item)) return;
      if (item.kind === "action" && item.def.argPrompt) {
        const history = recentValues(item.def.id);
        setModal({
          kind: "argPicker",
          slug: ap.slug,
          def: item.def,
          history,
          index: 0,
          input: history.length === 0 ? "" : null,
        });
        return;
      }
      if (item.kind === "action" && item.def.kind === "shell") {
        setModal(null);
        void launchAction(ap.slug, item.def, "");
        return;
      }
      const def = item.kind === "action" ? item.def : null;
      setModal({
        kind: "actionPicker",
        state: {
          mode: "edit",
          slug: ap.slug,
          def: def && def.kind === "claude" ? def : null,
          extras: "",
        },
      });
    };
    if (k.sequence === "c") {
      setModal({
        kind: "actionPicker",
        state: { mode: "edit", slug: ap.slug, def: null, extras: "" },
      });
      return true;
    }
    if (k.sequence && /^[a-z]$/.test(k.sequence)) {
      const i = items.findIndex(
        (it) => it.kind === "action" && it.key === k.sequence,
      );
      if (i >= 0) {
        commitIndex(i);
        return true;
      }
    }
    if (k.name === "return" || k.sequence === "!") {
      commitIndex(ap.index);
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

  if (k.ctrl && k.name === "c") {
    setModal(null);
    return true;
  }
  if (k.name === "escape") {
    const def = ap.def;
    if (def) {
      if (!rows.find((r) => r.wt.slug === ap.slug)) {
        setModal(null);
        toast("worktree gone", warnColor, 2000);
        return true;
      }
      const items = buildActionPickerItems(ap.slug);
      const idx = items.findIndex(
        (it) => it.kind === "action" && it.def.id === def.id,
      );
      setModal({
        kind: "actionPicker",
        state: { mode: "list", slug: ap.slug, index: Math.max(0, idx) },
      });
    } else {
      setModal(null);
    }
    return true;
  }
  if (k.name === "return") {
    const { slug, def, extras } = ap;
    setModal(null);
    void launchAction(slug, def, extras);
    return true;
  }
  if (k.name === "backspace") {
    if (ap.extras.length === 0) return true;
    setModal({
      kind: "actionPicker",
      state: { ...ap, extras: ap.extras.slice(0, -1) },
    });
    return true;
  }
  const text = printableMultiline(k.sequence);
  if (text) {
    setModal({
      kind: "actionPicker",
      state: { ...ap, extras: ap.extras + text },
    });
  }
  return true;
}

export function handleArgPickerKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "argPicker" }>,
  { setModal, launchAction }: SimpleModalContext,
): boolean {
  const rowCount = modal.history.length + 1;
  const isInput = modal.input !== null;
  const launch = (value: string): void => {
    const trimmed = value.trim();
    if (!trimmed) return;
    setModal(null);
    void launchAction(modal.slug, modal.def, "", trimmed);
  };
  if (k.ctrl && k.name === "c") {
    setModal(null);
    return true;
  }
  if (isInput) {
    if (k.name === "escape") {
      if (modal.history.length > 0) setModal({ ...modal, input: null, index: 0 });
      else setModal(null);
      return true;
    }
    if (k.name === "return") {
      launch(modal.input ?? "");
      return true;
    }
    if (k.name === "backspace") {
      setModal({ ...modal, input: (modal.input ?? "").slice(0, -1) });
      return true;
    }
    const text = printableMultiline(k.sequence);
    if (text) setModal({ ...modal, input: (modal.input ?? "") + text });
    return true;
  }
  if (k.name === "escape" || k.sequence === "q") {
    setModal(null);
    return true;
  }
  if (k.name === "j" || k.name === "down") {
    setModal({ ...modal, index: Math.min(modal.index + 1, rowCount - 1) });
    return true;
  }
  if (k.name === "k" || k.name === "up") {
    setModal({ ...modal, index: Math.max(modal.index - 1, 0) });
    return true;
  }
  if (k.sequence && /^[1-9]$/.test(k.sequence)) {
    const i = Number(k.sequence) - 1;
    if (i < modal.history.length) {
      const entry = modal.history[i];
      if (entry) launch(entry.value);
    }
    return true;
  }
  if (k.name === "return") {
    if (modal.index >= modal.history.length) {
      setModal({ ...modal, input: "" });
      return true;
    }
    const entry = modal.history[modal.index];
    if (entry) launch(entry.value);
  }
  return true;
}
