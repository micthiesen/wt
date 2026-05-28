import { resolve } from "node:path";

import { hideFrontmostAlacritty, openInZed } from "../core/zed.ts";

export { hideFrontmostAlacritty, openInZed };

/**
 * Path of the wt source tree itself. This file lives at
 * `<repo>/src/tui/helpers.ts`, so the repo root is two levels up
 * from `import.meta.dir`. Resolves consistently whether wt is
 * invoked through the bin shim or directly via `bun src/main.ts`.
 */
export const WT_REPO_PATH: string = resolve(import.meta.dir, "..", "..");

/** Fire-and-forget `open <url>`. The macOS `open` binary returns immediately. */
export function openUrl(url: string): void {
  Bun.spawn(["open", url], {
    stdin: "ignore",
    stdout: "ignore",
    stderr: "ignore",
  });
}

/**
 * Hide a frontmost Alacritty window, *then* open the URL. Order matters:
 * `openUrl` brings the browser to the front, while `hideFrontmostAlacritty`
 * shells out to `osascript` to sample the frontmost app and only sends
 * Cmd+H if it's Alacritty. Firing both without awaiting lets the browser
 * win the race — the frontmost query then sees the browser, not Alacritty,
 * and the hide no-ops. Awaiting the hide first keeps Alacritty frontmost
 * long enough to be detected and hidden. (Matters since the hide became
 * async; `openInZed` already sequences its own hide internally.)
 */
export async function openUrlHidingAlacritty(url: string): Promise<void> {
  await hideFrontmostAlacritty();
  openUrl(url);
}

/** Write to the macOS clipboard via pbcopy. Fire-and-forget. */
export function writeClipboard(text: string): void {
  const proc = Bun.spawn(["pbcopy"], {
    stdin: "pipe",
    stdout: "ignore",
    stderr: "ignore",
  });
  if (!proc.stdin) return;
  proc.stdin.write(text);
  proc.stdin.end();
}
