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
import { githubIssueUrl, issueUrlForId, resolveIssueId } from "../../core/issue-tracker.ts";
import type { RemovedWorktree } from "../../core/wtstate.ts";
import { isPlainLetter } from "../app-helpers.ts";
import { openUrlHidingTerminal } from "../../core/macos.ts";
import { operationErrors } from "../../core/errors.ts";
import { forkReported } from "../effect-boundary.ts";
import type { Modal } from "../modal-state.ts";
import { theme } from "../theme.ts";
import { Effect } from "effect";

const io = operationErrors("removed-view-keys");

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
  /** Ctrl+A on the archived row. Null when the slug left the history. */
  toggleRemovedAutomationsPaused: (slug: string) => Promise<boolean | null>;
};

export function handleRemovedViewKey(
  k: KeyEvent,
  ctx: RemovedViewKeysCtx,
): void {
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
    toggleRemovedAutomationsPaused,
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
  // Ctrl+A, same key as on a live row. The target is the removed
  // entry rather than `slugs`, which the reap already dropped —
  // and the automations still able to fire for an archived slug
  // (post-merge `external` runs) are exactly the ones this stops.
  if (k.ctrl && k.name === "a") {
    forkReported(
      io.promise("automations toggle", () => toggleRemovedAutomationsPaused(entry.slug)).pipe(
        Effect.tap((paused) =>
          Effect.sync(() => {
            if (paused === null) {
              toast("not in the archive record", theme.fgDim, 1500);
              return;
            }
            removedLog.event.info(
              paused
                ? "automations paused for this archived worktree"
                : "automations resumed for this archived worktree",
            );
            toast(
              paused
                ? `automations paused for ${entry.slug}`
                : `automations resumed for ${entry.slug}`,
              paused ? theme.warn : theme.ok,
              2000,
            );
          }),
        ),
      ),
      (error) =>
        removedLog.event.err(`automations toggle failed: ${error.message}`),
    );
    return;
  }
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
    const url = issueUrlForId(resolveIssueId(entry.slug, entry.issueId)) ??
      (entry.githubIssue ? githubIssueUrl(entry.githubIssue) : null);
    if (!url) {
      removedLog.event.warn(
        "no issue URL (needs a tracker URL or recorded GitHub issue)",
      );
      return;
    }
    forkReported(openUrlHidingTerminal(url), (error) =>
      removedLog.event.err(`open issue failed: ${error.message}`),
    );
    removedLog.event.info("opened issue");
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
