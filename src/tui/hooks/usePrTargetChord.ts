/**
 * `g p` / `l p` PR-target chord: `g`/`l` remembers a target for a short
 * window; a `p` inside the window opens the PR there instead of the
 * configured default. Extracted from `app.tsx` — owns the pending-chord
 * ref, its expiry timer, and the `p`-consumption check the keyboard
 * dispatcher calls early in normal mode.
 */
import { useRef } from "react";
import type { KeyEvent } from "@opentui/core";

import { config, type PullRequestTarget } from "../../core/config.ts";
import {
  pullRequestOpenUrl,
  pullRequestOpenUrlForTarget,
} from "../../core/github.ts";
import { createLogger } from "../../core/logger.ts";
import type { ReviewRequestPr } from "../../state/index.ts";
import { isPlainLetter } from "../app-helpers.ts";
import { openUrlHidingAlacritty } from "../../core/macos.ts";
import type { WorktreeRow } from "./useWorktreeRows.ts";

const PR_TARGET_CHORD_MS = 1_200;

type PendingPrTargetChord = {
  target: PullRequestTarget;
  url: string;
  number: number;
  logName: string;
  timer: Timer;
};

export function usePrTargetChord(opts: {
  selectedPr: ReviewRequestPr | undefined;
  current: WorktreeRow | undefined;
}) {
  const { selectedPr, current } = opts;
  const pendingPrTargetChordRef = useRef<PendingPrTargetChord | null>(null);

  function clearPendingPrTargetChord(): void {
    const pending = pendingPrTargetChordRef.current;
    if (pending) clearTimeout(pending.timer);
    pendingPrTargetChordRef.current = null;
  }

  function rememberPrTargetChord(target: PullRequestTarget): boolean {
    const pr = selectedPr ?? current?.pr;
    if (!pr) return false;
    clearPendingPrTargetChord();
    const logName = selectedPr ? "[review]" : current?.wt.slug ?? "[app]";
    const timer = setTimeout(() => {
      pendingPrTargetChordRef.current = null;
    }, PR_TARGET_CHORD_MS);
    pendingPrTargetChordRef.current = {
      target,
      url: pr.url,
      number: pr.number,
      logName,
      timer,
    };
    return true;
  }

  function openPrUrl(
    url: string,
    number: number,
    target: PullRequestTarget | null,
    logName: string,
  ): void {
    const resolved = target
      ? pullRequestOpenUrlForTarget(url, target)
      : pullRequestOpenUrl(url);
    const label = target ?? config.github.prTarget;
    void openUrlHidingAlacritty(resolved);
    createLogger(logName).event.info(`opened PR #${number} in ${label}`);
  }

  function consumePrTargetChord(k: KeyEvent): boolean {
    if (!isPlainLetter(k, "p")) return false;
    const pending = pendingPrTargetChordRef.current;
    if (!pending) return false;
    clearPendingPrTargetChord();
    openPrUrl(pending.url, pending.number, pending.target, pending.logName);
    return true;
  }

  return { rememberPrTargetChord, openPrUrl, consumePrTargetChord };
}
