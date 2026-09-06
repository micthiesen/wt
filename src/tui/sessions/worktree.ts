/**
 * Cross-session navigator for one worktree. The renderer is suspended once;
 * tmux's F-key bindings return a private switch result and this loop attaches
 * the requested target without flashing the wt home screen in between.
 */
import type { CliRenderer } from "@opentui/core";
import { Effect } from "effect";

import { getHarness, type HarnessId } from "../../core/harness/index.ts";
import { createLogger } from "../../core/logger.ts";
import {
  type AttachResult,
} from "../../core/tmux.ts";
import {
  AttachOperationError,
  attachOrCreate,
} from "../../core/tmux/attach.ts";
import { killHarnessSession } from "../../core/tmux/admin.ts";
import { workspaceSocket, showWorkspaceTarget, setWorkspaceTargetLabel } from "../../core/workspace.ts";
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

export type EnterWorktreeSessionOptions = {
  renderer: CliRenderer;
  slug: string;
  cwd: string;
  initial: WorktreeSessionTarget;
  diffBase: string;
  harness: HarnessRoute;
  /**
   * Whether the F10/F11/F12 tmux bindings may SWITCH between this
   * slug's shell/diff/harness sessions (the worktree navigator).
   * `false` for the special session slots (`,`/`.`/`/`/`m`): an F-key
   * there returns straight to wt instead of minting a stranded
   * `manager-diff`-style sibling for a non-worktree slug.
   */
  switchable?: boolean;
};

export function enterWorktreeSession(opts: EnterWorktreeSessionOptions) {
  const { renderer, ...target } = opts;
  if (workspaceSocket) {
    return showWorkspaceTarget(target).pipe(Effect.as({ kind: "detached" } as const));
  }
  return handoffTerminal(renderer, opts.cwd, navigateWorktreeSession(target));
}

/** Shared by fullscreen entry and the workspace's thin content host. */
export function navigateWorktreeSession(opts: Omit<EnterWorktreeSessionOptions, "renderer">) {
  const { slug, cwd, diffBase, harness, switchable = true } = opts;
  return Effect.gen(function* () {
    let target = opts.initial;
    let harnessPrepared = false;

    const attachTarget = (): Effect.Effect<AttachResult, AttachOperationError> => Effect.gen(function* () {
      if (target === "shell") {
        return yield* attachOrCreate({ slug, cwd, kind: "shell" });
      }
      if (target === "diff") {
        return yield* attachOrCreate({ slug, cwd, kind: "diff", base: diffBase });
      }

      if (!harnessPrepared) {
        harnessPrepared = true;
        if (harness.freshSlot && getHarness(harness.harnessId).singleSlot) {
          createLogger(slug).event.warn(
            `replacing ${getHarness(harness.harnessId).label} slot`,
          );
          yield* killHarnessSession(slug, harness.harnessId);
        }
      }
      return yield* attachOrCreate({
        slug,
        cwd,
        kind: harness.harnessId,
        managedName: harness.managedName,
        resumeSessionId: harness.resumeSessionId,
        claudeDisplayName: harness.claudeDisplayName,
      });
    });

    for (;;) {
      yield* setWorkspaceTargetLabel(opts, target);
      const result = yield* attachTarget();
      if (result.kind !== "switch") return result;
      if (!switchable) return { kind: "detached" } as const;
      target = result.target;
    }
  });
}
