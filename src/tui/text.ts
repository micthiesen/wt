/**
 * Shared text-formatting constants and helpers for the TUI.
 *
 * Use the same ellipsis glyph opentui's native `truncate` flag emits
 * (3-cell ASCII `...`). Mixing this and the 1-cell `…` in the same
 * pane reads as a font/encoding bug at a glance — keep them in sync.
 */
import { humanAge } from "../core/locks.ts";

export const ELLIPSIS = "...";
export const ELLIPSIS_WIDTH = 3;

/**
 * Format a millisecond delta as a human-readable age string ("12m",
 * "3h", "5d", …). Negative deltas clamp to zero — common when system
 * clocks drift relative to file timestamps.
 */
export function ageMsToText(ms: number): string {
  return humanAge(Math.max(0, ms) / 1000);
}

/**
 * End-truncate `s` to fit within `maxWidth` terminal cells, suffixing
 * `...` when it overflows. Trailing whitespace is stripped before the
 * suffix so a cut at a word boundary reads as `Do foo...` rather than
 * `Do foo ...`.
 *
 * Used in JS where opentui's native `truncate` flag isn't a fit
 * (middle-truncation, or layout that needs to know the final string
 * width). Glyph matches the native flag for visual consistency.
 */
export function truncateEnd(s: string, maxWidth: number): string {
  if (maxWidth <= 0) return "";
  if (Bun.stringWidth(s) <= maxWidth) return s;
  if (maxWidth < ELLIPSIS_WIDTH) return ELLIPSIS.slice(0, maxWidth);
  let cut = s;
  // Code-unit trim, deliberately NOT grapheme-aware: cutting through a
  // surrogate pair can leave a dangling half before the ellipsis. Known
  // and accepted — inputs here (slugs, branch names, titles) are
  // overwhelmingly ASCII and the worst case is one mojibake cell;
  // grapheme segmentation isn't worth it on this hot render path.
  while (cut.length > 0 && Bun.stringWidth(cut) + ELLIPSIS_WIDTH > maxWidth) {
    cut = cut.slice(0, -1);
  }
  return `${cut.trimEnd()}${ELLIPSIS}`;
}
