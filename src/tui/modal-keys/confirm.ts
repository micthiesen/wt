import type { KeyEvent } from "@opentui/core";

import { actionRegistry } from "../../core/actions.ts";
import { killDiffSession, killShellSession } from "../../core/tmux.ts";
import type { Modal } from "../modal.ts";
import type { SimpleModalContext } from "./ctx.ts";

export function handleKillActionConfirmKey(
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

export function handleKillSessionConfirmKey(
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

export function handleCleanConfirmKey(
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

export function handleConfirmKey(
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
    doRestoreRemoved,
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
    } else if (pending === "restore" && modal.restoreEntry) {
      void doRestoreRemoved(modal.restoreEntry);
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
