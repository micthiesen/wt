// Minimal ANSI color helpers. No deps.

const ESC = "\x1b[";
const RESET = `${ESC}0m`;

function wrap(code: number) {
  return (s: string): string => `${ESC}${code}m${s}${RESET}`;
}

export const isTty = Boolean(process.stdout.isTTY);

function plainIfNoTty(fn: (s: string) => string) {
  return (s: string): string => (isTty ? fn(s) : s);
}

export const cyan = plainIfNoTty(wrap(36));
export const green = plainIfNoTty(wrap(32));
export const yellow = plainIfNoTty(wrap(33));
export const red = plainIfNoTty(wrap(31));
export const blue = plainIfNoTty(wrap(34));
export const magenta = plainIfNoTty(wrap(35));
export const dim = plainIfNoTty(wrap(2));
export const bold = plainIfNoTty(wrap(1));

/** OSC 8 hyperlink. Falls back to plain text outside a TTY. */
export function link(label: string, url: string): string {
  if (!isTty) return label;
  return `\x1b]8;;${url}\x07${label}\x1b]8;;\x07`;
}

/** Strip ANSI escapes for width calculation. */
export function stripAnsi(s: string): string {
  // eslint-disable-next-line no-control-regex
  return s.replace(/\x1b\[[0-9;]*m/g, "").replace(/\x1b\].*?\x07/g, "");
}

export function visibleWidth(s: string): number {
  return [...stripAnsi(s)].length;
}
