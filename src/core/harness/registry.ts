/**
 * Registry of harness impls. Single source of truth for TAB-cycle
 * order (= array order here), F12 dispatch, and the sessions picker's
 * "+ new X" sub-affordances.
 *
 * Order is intentional: Claude first because it's the default primary
 * and the most feature-complete impl; Codex / OpenCode after because
 * they're partial-feature impls today (no busy/idle, no summaries).
 */
import { claudeHarness } from "./claude.ts";
import { codexHarness } from "./codex.ts";
import { opencodeHarness } from "./opencode.ts";
import type { Harness, HarnessId } from "./types.ts";

export const HARNESSES: readonly Harness[] = [
  claudeHarness,
  codexHarness,
  opencodeHarness,
];

const BY_ID = new Map<HarnessId, Harness>(HARNESSES.map((h) => [h.id, h]));

/** Look up a harness by id. Throws on unknown id — caller picks from the registry. */
export function getHarness(id: HarnessId): Harness {
  const h = BY_ID.get(id);
  if (!h) throw new Error(`unknown harness id: ${id}`);
  return h;
}

/**
 * Identify which harness owns a tmux session name (if any) for the
 * given slug. Walks impls in registration order and returns the first
 * match. Used by the tmux lister + reaper so harness-aware code can
 * route per-harness session names back to the right impl.
 */
export function detectHarnessFromTmuxName(
  name: string,
  slug: string,
): HarnessId | null {
  // Claude is special: its primary name is the bare slug, which would
  // also match an unrelated session name by accident. Check it via
  // the slug-aware infix rules in claudeHarness first.
  // Order: codex/opencode have unambiguous `-<id>~` infix, claude is
  // anything else that matches slug or slug~.
  if (name.startsWith(`${slug}-codex~`)) return "codex";
  if (name.startsWith(`${slug}-opencode~`)) return "opencode";
  if (name === slug) return "claude";
  if (name.startsWith(`${slug}~`)) return "claude";
  return null;
}
