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
  try {
    return await attachOrCreate({ slug, cwd });
  } finally {
    renderer.resume();
  }
}
