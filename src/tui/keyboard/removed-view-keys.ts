/**
 * Removed-worktrees view (`h`): the left pane shows destroy history
 * instead of live rows. Its own small key map — navigation, the
 * PR/issue/yank carryovers, and Enter-to-restore — and everything else
 * is swallowed so worktree-keyed actions can't fire against the hidden
 * live selection. Extracted from `app.tsx`.
 */
import type { KeyEvent } from "@opentui/core";

import { config } from "../../core/config.ts";
import { createLogger } from "../../core/logger.ts";
import { linearUrlForSlug } from "../../core/linear.ts";
import type { RemovedWorktree } from "../../core/wtstate.ts";
import { isPlainLetter } from "../app-helpers.ts";
import { openUrlHidingAlacritty } from "../../core/macos.ts";
import type { Modal } from "../modal.ts";
import { theme } from "../theme.ts";

export type RemovedViewKeysCtx = {
  setRemovedView: (v: boolean) => void;
  handleGlobalKey: (k: KeyEvent) => boolean;
  removedEntries: readonly RemovedWorktree[];
  removedCursor: number;
  setRemovedIndex: (i: number) => void;
  openPrUrl: (
    url: string,
    number: number,
    target: null,
    logName: string,
  ) => void;
  doYank: (slug: string, label: string, value: string | null) => void;
  setModal: (m: Modal | null) => void;
  toast: (message: string, color?: string, ms?: number) => void;
};

export function handleRemovedViewKey(k: KeyEvent, ctx: RemovedViewKeysCtx): void {
  const {
    setRemovedView,
    handleGlobalKey,
    removedEntries,
    removedCursor,
    setRemovedIndex,
    openPrUrl,
    doYank,
    setModal,
    toast,
  } = ctx;
      if (k.name === "escape" || isPlainLetter(k, "h")) {
        setRemovedView(false);
        return;
      }
      if (handleGlobalKey(k)) return;
      if (k.name === "j" || k.name === "down") {
        setRemovedIndex(
          Math.min(removedCursor + 1, Math.max(0, removedEntries.length - 1)),
        );
        return;
      }
      if (k.name === "k" || k.name === "up") {
        setRemovedIndex(Math.max(0, removedCursor - 1));
        return;
      }
      if (k.sequence === "g") {
        setRemovedIndex(0);
        return;
      }
      if (k.sequence === "G") {
        setRemovedIndex(Math.max(0, removedEntries.length - 1));
        return;
      }
      const entry = removedEntries[removedCursor];
      if (!entry) return;
      const removedLog = createLogger(entry.slug);
      if (isPlainLetter(k, "p")) {
        if (!entry.prUrl) {
          removedLog.event.warn("no PR recorded for this branch");
          toast("no PR recorded", theme.fgDim, 1500);
          return;
        }
        openPrUrl(entry.prUrl, entry.prNumber ?? 0, null, entry.slug);
        return;
      }
      if (isPlainLetter(k, "i")) {
        const url = linearUrlForSlug(entry.slug);
        if (!url) {
          removedLog.event.warn("no linear id in slug");
          return;
        }
        void openUrlHidingAlacritty(url);
        removedLog.event.info("opened linear");
        return;
      }
      if (k.sequence === "y") {
        doYank(entry.slug, "branch", entry.branch);
        return;
      }
      if (k.name === "return") {
        setModal({
          kind: "confirm",
          pendingKey: "restore",
          restoreEntry: entry,
          title: "restore worktree",
          message: `Restore ${entry.slug}?`,
          detail: `Creates a worktree for ${entry.branch} (checked out if the branch still exists, fresh off ${config.branch.base} otherwise).`,
          confirmLabel: "restore",
        });
        return;
      }
      return;
}
