import { resolve } from "node:path";

export { hideFrontmostAlacritty, openInZed } from "../core/zed.ts";

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
