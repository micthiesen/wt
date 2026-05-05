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

import { attachOrCreate, type AttachResult } from "../core/tmux.ts";

export type ShellResult = AttachResult;

const CLEAR_SCREEN = "\x1b[2J\x1b[H";

export async function enterShellSession(opts: {
  renderer: CliRenderer;
  slug: string;
  cwd: string;
}): Promise<ShellResult> {
  const { renderer, slug, cwd } = opts;
  renderer.suspend();
  process.stdout.write(CLEAR_SCREEN);
  try {
    return await attachOrCreate({ slug, cwd, kind: "shell" });
  } finally {
    process.stdout.write(CLEAR_SCREEN);
    renderer.resume();
  }
}
