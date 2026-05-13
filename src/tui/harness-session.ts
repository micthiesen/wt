/**
 * Renderer-side orchestrator for entering an interactive AI harness
 * session. Suspends the opentui renderer, hands the terminal off to
 * `core/tmux.attachOrCreate`, and resumes when the tmux client exits.
 *
 * The harness id is passed through directly; argv construction lives
 * in the harness impl (`core/harness/<id>.ts`). All tmux mechanics
 * (config, server lifecycle, env stripping, session naming) live in
 * `core/tmux.ts` — this module only knows about the renderer handoff
 * and the result types the keyboard handler needs.
 */
import type { CliRenderer } from "@opentui/core";

import { type HarnessId } from "../core/harness/index.ts";
import { attachOrCreate, type AttachResult } from "../core/tmux.ts";

export type EnterResult = AttachResult;

/**
 * Clear-screen + cursor-home. opentui's suspend emits `\x1b[?1049l`
 * which drops the terminal back to its main screen, briefly exposing
 * whatever was there before wt started. Painting a clean screen into
 * that brief window replaces the pre-wt scroll with a uniform black
 * gap so the F12 transition reads as a clean cut rather than a flash
 * of unrelated content. Symmetric on resume so the same window on the
 * way back doesn't reveal the harness's last frame either.
 */
const CLEAR_SCREEN = "\x1b[2J\x1b[H";

export async function enterHarnessSession(opts: {
  renderer: CliRenderer;
  slug: string;
  cwd: string;
  /** Which AI harness to spawn (or attach to). */
  harnessId: HarnessId;
  /**
   * Claude-only: null = primary tmux slot (`<slug>`), string = named
   * additional session (`<slug>~<name>`). Codex / OpenCode ignore the
   * name for tmux naming (single-tmux-per-slug) but pass it through
   * to the harness's buildArgs.
   */
  managedName?: string | null;
  /**
   * Harness session id to resume. `null` (or omitted) spawns fresh.
   * The harness impl decides what "fresh" means — claude derives a
   * deterministic UUID from (slug, name); codex / opencode let their
   * own CLI generate one.
   */
  resumeSessionId?: string | null;
  /**
   * Claude primary only — label in `/resume` listings. Defaults to
   * "primary". The wt-source-repo `.` shortcut passes the source slug.
   */
  claudeDisplayName?: string;
}): Promise<EnterResult> {
  const {
    renderer,
    slug,
    cwd,
    harnessId,
    managedName,
    resumeSessionId,
    claudeDisplayName,
  } = opts;
  renderer.suspend();
  process.stdout.write(CLEAR_SCREEN);
  try {
    return await attachOrCreate({
      slug,
      cwd,
      kind: harnessId,
      managedName,
      resumeSessionId,
      claudeDisplayName,
    });
  } finally {
    process.stdout.write(CLEAR_SCREEN);
    renderer.resume();
  }
}
