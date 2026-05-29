/**
 * Theme + glyph mapping for the `DerivedState` values, shared by every
 * surface that displays AI-session state: the details-pane row, the
 * footer slots, the list-pane glyph, and the sessions picker. Single
 * source of truth so a color or glyph change lands in one place.
 *
 * Color is per-harness: each harness keys its own `DerivedState ->
 * color` table via {@link stateColor}. `waiting` takes the harness brand
 * color and `working` its complement, so a glance reads vendor *and*
 * state through a contrasting hue pair (claude cyan/orange, codex
 * amber/indigo, opencode lime/violet). The remaining states stay
 * semantic and shared: `abandoned` is always red
 * (crashed is crashed) and `idle` always dim, while `asking`/`polling`/
 * `unknown` come only from claude's registry — codex/opencode never
 * emit them — so they keep claude's values everywhere for exhaustiveness.
 *
 * The glyph table ({@link STATE_DOT}) stays shared: shape encodes state,
 * color encodes harness + state together.
 */
import type { HarnessId } from "../core/harness/types.ts";
import type { DerivedState } from "../core/claude-status.ts";

import { theme } from "./theme.ts";

// States that read the same across every harness. `asking` is magenta
// ("blocked on you, look here"), `polling` muted teal ("backgrounded
// work in flight"), `unknown` muted blue ("alive but indeterminate"),
// `abandoned` red (a crashed session is a crashed session regardless of
// vendor), and `idle` shares fgDim with the empty-state `—` (a ghost
// session reads as "nothing happening", not a warning).
const SHARED: Pick<
  Record<DerivedState, string>,
  "asking" | "polling" | "unknown" | "abandoned" | "idle"
> = {
  asking: theme.info,
  polling: theme.teal,
  unknown: theme.accentAlt,
  abandoned: theme.err,
  idle: theme.fgDim,
};

export const STATE_FG_BY_HARNESS: Record<
  HarnessId,
  Record<DerivedState, string>
> = {
  // `waiting` ("your move") takes the harness brand color everywhere;
  // `working` takes that brand's complement, so the two states contrast
  // in hue (claude cyan/orange, codex amber/indigo, opencode lime/violet)
  // rather than differing only in brightness.
  claude: { working: theme.accent, waiting: theme.claude, ...SHARED },
  codex: { working: theme.codexAlt, waiting: theme.codex, ...SHARED },
  opencode: {
    working: theme.opencodeAlt,
    waiting: theme.opencode,
    ...SHARED,
  },
};

/** Per-harness foreground for a `DerivedState`. */
export function stateColor(harnessId: HarnessId, state: DerivedState): string {
  return STATE_FG_BY_HARNESS[harnessId][state];
}

/**
 * Claude's palette, kept as a standalone export for the harness-agnostic
 * help legend (which illustrates state semantics, not a specific vendor)
 * and any surface that has a state but no harness in hand.
 */
export const STATE_FG: Record<DerivedState, string> =
  STATE_FG_BY_HARNESS.claude;

export const STATE_DOT: Record<DerivedState, string> = {
  working: "●",
  asking: "?",
  polling: "↻",
  unknown: "◌",
  waiting: "○",
  abandoned: "✕",
  idle: "·",
};
