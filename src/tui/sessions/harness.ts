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

import type { HarnessId } from "../../core/harness/index.ts";
import {
  enterWorktreeSession,
  type WorktreeSessionResult,
} from "./worktree.ts";

export type EnterResult = WorktreeSessionResult;

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
  /**
   * Codex / OpenCode only: ensure the single-tmux-per-slug slot starts
   * fresh by killing any existing slot before attaching. Needed for
   * "+ new" and for "resume a specific dead session" — without it,
   * `tmux new-session -A` silently attaches to whatever's already in
   * the slot and the harness argv (`codex` / `codex resume <id>` /
   * `opencode -s <id>`) is ignored. Claude has per-name tmux slots so
   * it never needs this; the flag is ignored for `harnessId === claude`.
   */
  freshSlot?: boolean;
  /** Resolved base used if F11 is pressed while inside the harness. */
  diffBase: string;
}): Promise<EnterResult> {
  const {
    renderer,
    slug,
    cwd,
    harnessId,
    managedName,
    resumeSessionId,
    claudeDisplayName,
    freshSlot,
    diffBase,
  } = opts;
  return await enterWorktreeSession({
    renderer,
    slug,
    cwd,
    initial: "harness",
    diffBase,
    harness: {
      harnessId,
      managedName,
      resumeSessionId,
      claudeDisplayName,
      freshSlot,
    },
  });
}
