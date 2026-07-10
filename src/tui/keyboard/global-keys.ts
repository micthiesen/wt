/**
 * App-level keys that work in BOTH list views — the normal worktree
 * list and the removed-worktrees history (`h`). Anything keyed off the
 * selected row / section / PR stays in the normal-mode handler; this is
 * only the stuff with no per-row context: help, quit, refresh, cache
 * clear, new/clean, the global automations pause, the primary-harness
 * cycle, and the slot sessions / zed opens. Extracted from `app.tsx`.
 */
import type { KeyEvent } from "@opentui/core";

import { getHarness, type HarnessId } from "../../core/harness/index.ts";
import { createLogger } from "../../core/logger.ts";
import { isPlainLetter, isShiftedLetter } from "../app-helpers.ts";
import { openInZed } from "../helpers.ts";
import type { Modal } from "../modal.ts";
import type { FooterMode } from "../panels/footer.tsx";
import {
  DOTFILES_SLOT,
  MAIN_CLONE_SLOT,
  WT_SOURCE_SLOT,
  type SessionSlot,
} from "../sessions/slots.ts";
import { theme } from "../theme.ts";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";

const appLog = createLogger("[app]");
const newLog = createLogger("[new]");
const wtSourceLog = createLogger(WT_SOURCE_SLOT.label);

export type GlobalKeysCtx = {
  setModal: (m: Modal | null) => void;
  quit: () => void;
  refreshAll: () => Promise<void>;
  setFooter: (f: FooterMode) => void;
  cleanCandidates: WorktreeRow[];
  toast: (message: string, color?: string, ms?: number) => void;
  reportActionError: (label: string, err: unknown) => void;
  automations: { configured: boolean; togglePaused: () => Promise<boolean> };
  cyclePrimaryHarness: () => Promise<HarnessId>;
  doEnterSlotSession: (slot: SessionSlot) => void;
};

export function handleGlobalKey(k: KeyEvent, ctx: GlobalKeysCtx): boolean {
  const {
    setModal,
    quit,
    refreshAll,
    setFooter,
    cleanCandidates,
    toast,
    reportActionError,
    automations,
    cyclePrimaryHarness,
    doEnterSlotSession,
  } = ctx;
    if (k.sequence === "?") {
      setModal({ kind: "help", query: "", searching: false });
      return true;
    }
    if (isPlainLetter(k, "q") || (k.ctrl && k.name === "c")) {
      quit();
      return true;
    }
    if (k.sequence === "r") {
      appLog.event.dim("refresh");
      void refreshAll();
      return true;
    }
    // Ctrl+R: clear all caches. Moved off bare R when R lost its
    // single-letter slot; same confirm flow, same handler.
    if (k.ctrl && k.name === "r") {
      setModal({
        kind: "confirm",
        pendingKey: "R",
        title: "clear caches",
        message: "Clear all cached data and refetch from scratch?",
        confirmLabel: "clear",
      });
      return true;
    }
    if (k.sequence === "n") {
      newLog.event.dim("tip: --any to match any author, --base <ref> to branch off");
      setFooter({ kind: "input", prompt: "new:", value: "", purpose: "new" });
      return true;
    }
    if (isPlainLetter(k, "c")) {
      if (cleanCandidates.length === 0) {
        toast("nothing to clean", theme.fgDim, 1500);
        return true;
      }
      setModal({ kind: "cleanConfirm" });
      return true;
    }
    // Shift+A — pause / resume ALL automations. Persisted in wtstate,
    // so the pause survives restarts. The pending intent queue is
    // dropped on pause; conditions that still hold re-derive it on
    // resume.
    if (isShiftedLetter(k, "a")) {
      if (!automations.configured) {
        toast("no [[automations]] configured", theme.fgDim, 2000);
        return true;
      }
      void automations.togglePaused().then(
        (nowPaused) => {
          toast(
            nowPaused ? "automations paused" : "automations resumed",
            nowPaused ? theme.warn : theme.ok,
            2000,
          );
        },
        (err) => reportActionError("automations toggle", err),
      );
      return true;
    }
    // Shift+TAB — cycle the primary harness selection. Re-rendered top-
    // right indicator reflects the new primary; subsequent F12 spawns
    // pick it up.
    if (
      k.name === "tab" &&
      k.shift &&
      !k.ctrl &&
      !k.option &&
      !k.super &&
      !k.hyper &&
      !k.meta
    ) {
      void (async () => {
        const next = await cyclePrimaryHarness();
        appLog.event.info(`primary harness → ${getHarness(next).label}`);
      })();
      return true;
    }
    // Toggle into a persistent harness session for a session slot —
    // `,` is the wt source repo (config/self edits), `.` is the
    // configured main clone, `/` the dotfiles. Same model as F12 on a
    // worktree row: tmux's `new-session -A` makes re-entry idempotent,
    // and F12 (bound to detach-client in the wt-private tmux config)
    // takes the user back out. The selected primary harness (TAB to
    // cycle) is the spawned kind, mirroring how row F12 picks a harness.
    if (k.sequence === ",") {
      doEnterSlotSession(WT_SOURCE_SLOT);
      return true;
    }
    if (k.sequence === ".") {
      doEnterSlotSession(MAIN_CLONE_SLOT);
      return true;
    }
    if (k.sequence === "/") {
      doEnterSlotSession(DOTFILES_SLOT);
      return true;
    }
    if (k.sequence === ">") {
      openInZed(WT_SOURCE_SLOT.path);
      wtSourceLog.event.info(`opened ${WT_SOURCE_SLOT.path}`);
      return true;
    }
    if (k.sequence === "O") {
      openInZed(MAIN_CLONE_SLOT.path);
      createLogger(MAIN_CLONE_SLOT.label).event.info(
        `opened ${MAIN_CLONE_SLOT.path}`,
      );
      return true;
    }
    return false;
}
