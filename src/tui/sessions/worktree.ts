/**
 * Cross-session navigator for one worktree. The renderer is suspended once;
 * tmux's F-key bindings return a private switch result and this loop attaches
 * the requested target without flashing the wt home screen in between.
 */
import type { CliRenderer } from "@opentui/core";

import { getHarness, type HarnessId } from "../../core/harness/index.ts";
import { createLogger } from "../../core/logger.ts";
import {
  attachOrCreate,
  killHarnessSession,
  type AttachResult,
} from "../../core/tmux.ts";
import { handoffTerminal } from "./renderer-handoff.ts";

export type HarnessRoute = {
  harnessId: HarnessId;
  managedName?: string | null;
  resumeSessionId?: string | null;
  claudeDisplayName?: string;
  freshSlot?: boolean;
};

export type WorktreeSessionTarget = "shell" | "diff" | "harness";
export type WorktreeSessionResult = Exclude<AttachResult, { kind: "switch" }>;

export async function enterWorktreeSession(opts: {
  renderer: CliRenderer;
  slug: string;
  cwd: string;
  initial: WorktreeSessionTarget;
  diffBase: string;
  harness: HarnessRoute;
}): Promise<WorktreeSessionResult> {
  const { renderer, slug, cwd, diffBase, harness } = opts;
  let target = opts.initial;
  let harnessPrepared = false;

  async function attachTarget(): Promise<AttachResult> {
    if (target === "shell") {
      return await attachOrCreate({ slug, cwd, kind: "shell" });
    }
    if (target === "diff") {
      return await attachOrCreate({ slug, cwd, kind: "diff", base: diffBase });
    }

    if (!harnessPrepared) {
      harnessPrepared = true;
      if (harness.freshSlot && getHarness(harness.harnessId).singleSlot) {
        createLogger(slug).event.warn(
          `replacing ${getHarness(harness.harnessId).label} slot`,
        );
        await killHarnessSession(slug, harness.harnessId);
      }
    }
    return await attachOrCreate({
      slug,
      cwd,
      kind: harness.harnessId,
      managedName: harness.managedName,
      resumeSessionId: harness.resumeSessionId,
      claudeDisplayName: harness.claudeDisplayName,
    });
  }

  return await handoffTerminal(renderer, async () => {
    for (;;) {
      const result = await attachTarget();
      if (result.kind !== "switch") return result;
      target = result.target;
    }
  });
}
