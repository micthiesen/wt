/**
 * Theme + glyph mapping for the four `DerivedState` values, shared by
 * every surface that displays claude-session state: the details-pane
 * row, the picker, and the list-pane session-count glyph. Single
 * source of truth so a color or glyph change lands in one place.
 *
 * `STATE_FG` is the canonical label palette. The list-pane glyph uses
 * `STATE_FG_GLYPH` instead, which preserves Claude's brand-orange for
 * the `waiting` state — the count badge has always been orange, and
 * uniform "amber" coloring would lose the "this row has claude
 * sessions" recognition that the list pane relies on.
 */
import type { DerivedState } from "../core/claude-status.ts";

import { theme } from "./theme.ts";

export const STATE_FG: Record<DerivedState, string> = {
  working: theme.accent,
  waiting: theme.warn,
  abandoned: theme.err,
  // `idle` shares fgDim with the empty-state `—` — a ghost session is
  // visually closer to "nothing happening" than to a warning.
  idle: theme.fgDim,
};

export const STATE_FG_GLYPH: Record<DerivedState, string> = {
  ...STATE_FG,
  waiting: theme.claudeOrange,
};

export const STATE_DOT: Record<DerivedState, string> = {
  working: "●",
  waiting: "○",
  abandoned: "✕",
  idle: "·",
};
