/**
 * Renderer-side orchestrator for entering an interactive `claude`
 * session. Suspends the opentui renderer, hands the terminal off to
 * `core/tmux.attachOrCreate`, and resumes when the tmux client exits.
 *
 * All tmux mechanics (config, server lifecycle, env stripping, session
 * naming) live in `core/tmux.ts` — this module only knows about the
 * renderer handoff and the result types the keyboard handler needs.
 */
import type { CliRenderer } from "@opentui/core";

import { attachOrCreate, type AttachResult } from "../core/tmux.ts";

export type EnterResult = AttachResult;

/**
 * Clear-screen + cursor-home. opentui's suspend emits `\x1b[?1049l`
 * which drops the terminal back to its main screen, briefly exposing
 * whatever was there before wt started. Painting a clean screen into
 * that brief window replaces the pre-wt scroll with a uniform black
 * gap so the F12 transition reads as a clean cut rather than a flash
 * of unrelated content. Symmetric on resume so the same window on the
 * way back doesn't reveal claude's last frame either.
 */
const CLEAR_SCREEN = "\x1b[2J\x1b[H";

export async function enterClaudeSession(opts: {
  renderer: CliRenderer;
  slug: string;
  cwd: string;
}): Promise<EnterResult> {
  const { renderer, slug, cwd } = opts;
  // try/finally so a spawn-failed throw can't strand the UI in the
  // suspended state. attachOrCreate already swallows spawn errors and
  // returns `spawn-failed`, but defending against future regressions
  // is cheap.
  renderer.suspend();
  process.stdout.write(CLEAR_SCREEN);
  try {
    return await attachOrCreate({ slug, cwd, kind: "claude" });
  } finally {
    process.stdout.write(CLEAR_SCREEN);
    renderer.resume();
  }
}
