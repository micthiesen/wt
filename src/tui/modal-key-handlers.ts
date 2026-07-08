import type { Dispatch, SetStateAction } from "react";
import type { KeyEvent } from "@opentui/core";

import { actionRegistry } from "../core/actions.ts";
import { killDiffSession, killShellSession } from "../core/tmux.ts";
import { isPlainLetter } from "./app-helpers.ts";
import type { Modal } from "./modal.ts";
import { yankItemsFor } from "./panels/yank.tsx";
import type { WorktreeRow } from "./hooks/useWorktreeRows.ts";

type SimpleModalContext = {
  setModal: Dispatch<SetStateAction<Modal | null>>;
  current: WorktreeRow | undefined;
  refreshTmuxSessions: () => Promise<unknown>;
  commitBasePick: (
    item: { label: string; branch: string | null },
    slug: string,
  ) => void;
  doYank: (slug: string, label: string, value: string | null) => void;
  doClean: () => void;
  doRemove: (slug: string, opts?: { force?: boolean }) => Promise<void>;
  doAutoMerge: (slug: string, mode: "enable" | "disable") => Promise<void>;
  doMarkReady: (slug: string) => Promise<void>;
  doShipPr: (slug: string) => Promise<void>;
  doCheckoutReview: (branch: string) => Promise<void>;
  clearAll: () => Promise<void>;
  logWarn: (message: string) => void;
  logErr: (message: string) => void;
};

export function handleSimpleModalKey(
  k: KeyEvent,
  modal: Modal,
  ctx: SimpleModalContext,
): boolean {
  switch (modal.kind) {
    case "killActionConfirm":
      return handleKillActionConfirmKey(k, modal, ctx);
    case "killSessionConfirm":
      return handleKillSessionConfirmKey(k, modal, ctx);
    case "branchPicker":
      return handleBranchPickerKey(k, modal, ctx);
    case "basePicker":
      return handleBasePickerKey(k, modal, ctx);
    case "yank":
      return handleYankKey(k, ctx);
    case "cleanConfirm":
      return handleCleanConfirmKey(k, ctx);
    case "confirm":
      return handleConfirmKey(k, modal, ctx);
    default:
      return false;
  }
}

function handleKillActionConfirmKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "killActionConfirm" }>,
  { setModal, logWarn }: SimpleModalContext,
): boolean {
  if (k.name === "y" || k.name === "return") {
    const { slug, actionName } = modal;
    setModal(null);
    void actionRegistry.kill(slug).then((killed) => {
      if (killed) logWarn(`killed action "${actionName}" on ${slug}`);
    });
    return true;
  }
  if (
    k.name === "n" ||
    k.name === "escape" ||
    k.sequence === "!" ||
    k.sequence === "q" ||
    (k.ctrl && k.name === "c")
  ) {
    setModal(null);
  }
  return true;
}

function handleKillSessionConfirmKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "killSessionConfirm" }>,
  { setModal, refreshTmuxSessions, logWarn, logErr }: SimpleModalContext,
): boolean {
  if (k.name === "y" || k.name === "return") {
    const { slug, sessionKind } = modal;
    setModal(null);
    const kill = sessionKind === "diff" ? killDiffSession : killShellSession;
    void kill(slug)
      .then(() => {
        logWarn(`killed ${sessionKind} session on ${slug}`);
        void refreshTmuxSessions();
      })
      .catch((err) => {
        const msg = err instanceof Error ? err.message : String(err);
        logErr(`kill ${sessionKind} session failed for ${slug}: ${msg}`);
      });
    return true;
  }
  if (
    k.name === "n" ||
    k.name === "escape" ||
    k.sequence === "q" ||
    (k.ctrl && k.name === "c")
  ) {
    setModal(null);
  }
  return true;
}

function handleBranchPickerKey(
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

function handleBasePickerKey(
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

function handleYankKey(
  k: KeyEvent,
  { setModal, current, doYank }: SimpleModalContext,
): boolean {
  if (
    k.name === "escape" ||
    k.sequence === "y" ||
    k.sequence === "q" ||
    (k.ctrl && k.name === "c")
  ) {
    setModal(null);
    return true;
  }
  if (current) {
    const item = yankItemsFor(current).find((it) => it.key === k.sequence);
    if (item) {
      setModal(null);
      doYank(current.wt.slug, item.label, item.value);
    }
  }
  return true;
}

function handleCleanConfirmKey(
  k: KeyEvent,
  { setModal, doClean }: SimpleModalContext,
): boolean {
  if (k.name === "y" || k.name === "return") {
    setModal(null);
    void doClean();
    return true;
  }
  if (
    k.name === "n" ||
    k.name === "escape" ||
    k.sequence === "q" ||
    (k.ctrl && k.name === "c")
  ) {
    setModal(null);
  }
  return true;
}

function handleConfirmKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "confirm" }>,
  ctx: SimpleModalContext,
): boolean {
  const {
    setModal,
    current,
    doRemove,
    doAutoMerge,
    doMarkReady,
    doShipPr,
    doCheckoutReview,
    clearAll,
    logWarn,
  } = ctx;
  if (k.name === "y" || k.name === "return") {
    const pending = modal.pendingKey;
    setModal(null);
    if (pending === "d" && current) {
      void doRemove(current.wt.slug);
    } else if (pending === "d!" && current) {
      void doRemove(current.wt.slug, { force: true });
    } else if (pending === "m+" && current) {
      void doAutoMerge(current.wt.slug, "enable");
    } else if (pending === "m-" && current) {
      void doAutoMerge(current.wt.slug, "disable");
    } else if (pending === "e" && current) {
      void doMarkReady(current.wt.slug);
    } else if (pending === "E" && current) {
      void doShipPr(current.wt.slug);
    } else if (pending === "review-wt" && modal.reviewBranch) {
      void doCheckoutReview(modal.reviewBranch);
    } else if (pending === "R") {
      logWarn("cleared all cached data; refetching from scratch");
      void clearAll();
    }
    return true;
  }
  if (
    k.name === "n" ||
    k.name === "escape" ||
    k.sequence === "q" ||
    (k.ctrl && k.name === "c")
  ) {
    setModal(null);
  }
  return true;
}
