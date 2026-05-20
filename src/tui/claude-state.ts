/**
 * Theme + glyph mapping for the `DerivedState` values, shared by every
 * surface that displays AI-session state: the details-pane row and the
 * sessions picker. Single source of truth so a color or glyph change
 * lands in one place.
 */
import type { DerivedState } from "../core/claude-status.ts";

import { theme } from "./theme.ts";

export const STATE_FG: Record<DerivedState, string> = {
  working: theme.accent,
  // `asking` = claude is blocked on you right now. Magenta so it stands
  // apart from working (cyan) and abandoned (red) and reads as "look here".
  asking: theme.info,
  // `unknown` = live session, status we don't recognize. Muted blue so
  // it reads as "alive but indeterminate", distinct from working (cyan).
  unknown: theme.accentAlt,
  // `waiting` = turn done, your move. Anthropic brand orange.
  waiting: theme.claude,
  abandoned: theme.err,
  // `idle` shares fgDim with the empty-state `—` — a ghost session is
  // visually closer to "nothing happening" than to a warning.
  idle: theme.fgDim,
};

export const STATE_DOT: Record<DerivedState, string> = {
  working: "●",
  asking: "?",
  unknown: "◌",
  waiting: "○",
  abandoned: "✕",
  idle: "·",
};
