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

import { getHarness, type HarnessId } from "../core/harness/index.ts";
import { createLogger } from "../core/logger.ts";
import {
  attachOrCreate,
  killHarnessSession,
  type AttachResult,
} from "../core/tmux.ts";
import { handoffTerminal } from "./renderer-handoff.ts";

export type EnterResult = AttachResult;

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
  } = opts;
  // Kill the single-slot tmux before suspending the renderer so (a)
  // the user sees the activity event in the still-rendered pane
  // rather than a black screen during the kill, and (b) the
  // subsequent `tmux new-session -A` always lands on a clean slot
  // and our buildArgs argv actually runs. Single-slot semantics are
  // codex/opencode only — claude already gets a unique tmux name
  // per managedName so the flag is a no-op there.
  if (freshSlot && getHarness(harnessId).singleSlot) {
    createLogger(slug).event.warn(
      `replacing ${getHarness(harnessId).label} slot`,
    );
    await killHarnessSession(slug, harnessId);
  }
  return await handoffTerminal(renderer, () =>
    attachOrCreate({
      slug,
      cwd,
      kind: harnessId,
      managedName,
      resumeSessionId,
      claudeDisplayName,
    }),
  );
}
