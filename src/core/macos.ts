/**
 * macOS-only process utilities (`open`, `pbcopy`) — part of the macOS
 * assumption noted in the README.
 */
import { hideFrontmostAlacritty } from "./zed.ts";

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
