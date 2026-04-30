/**
 * Shared text-formatting constants and helpers for the TUI.
 *
 * Use the same ellipsis glyph opentui's native `truncate` flag emits
 * (3-cell ASCII `...`). Mixing this and the 1-cell `…` in the same
 * pane reads as a font/encoding bug at a glance — keep them in sync.
 */
export const ELLIPSIS = "...";
export const ELLIPSIS_WIDTH = 3;

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
  while (cut.length > 0 && Bun.stringWidth(cut) + ELLIPSIS_WIDTH > maxWidth) {
    cut = cut.slice(0, -1);
  }
  return `${cut.trimEnd()}${ELLIPSIS}`;
}
