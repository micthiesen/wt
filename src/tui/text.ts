/**
 * Shared text-formatting constants for the TUI.
 *
 * Use the same ellipsis glyph opentui's native `truncate` flag emits
 * (3-cell ASCII `...`). Mixing this and the 1-cell `…` in the same
 * pane reads as a font/encoding bug at a glance — keep them in sync.
 */
export const ELLIPSIS = "...";
export const ELLIPSIS_WIDTH = 3;
