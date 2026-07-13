/**
 * Renderer-side orchestrator for the F10 plain-shell session.
 * Suspends the opentui renderer, hands the terminal off to a
 * persistent tmux session running the user's login shell in the
 * worktree's cwd, and resumes when the tmux client exits.
 *
 * Persistent like the F12 claude session: detach with F10, reattach
 * to find scrollback, env, and any background processes still in
 * place. Quitting the shell (`exit` / Ctrl+D) ends the session.
 */
import type { CliRenderer } from "@opentui/core";

import {
  enterWorktreeSession,
  type HarnessRoute,
  type WorktreeSessionResult,
} from "./worktree.ts";

export type ShellResult = WorktreeSessionResult;

export async function enterShellSession(opts: {
  renderer: CliRenderer;
  slug: string;
  cwd: string;
  diffBase: string;
  harness: HarnessRoute;
}): Promise<ShellResult> {
  return await enterWorktreeSession({ ...opts, initial: "shell" });
}
