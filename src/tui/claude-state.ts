/**
 * Theme + glyph mapping for the four `DerivedState` values, shared by
 * every surface that displays AI-session state: the details-pane row
 * and the sessions picker. Single source of truth so a color or glyph
 * change lands in one place.
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

export const STATE_DOT: Record<DerivedState, string> = {
  working: "●",
  waiting: "○",
  abandoned: "✕",
  idle: "·",
};
