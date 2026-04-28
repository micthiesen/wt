import { spawn } from "node:child_process";
import { resolve } from "node:path";

export { hideFrontmostAlacritty, openInZed } from "../core/zed.ts";

/**
 * Path of the wt source tree itself. This file lives at
 * `<repo>/src/tui/helpers.ts`, so the repo root is two levels up
 * from `import.meta.dir`. Resolves consistently whether wt is
 * invoked through the bin shim or directly via `bun src/main.ts`.
 */
export const WT_REPO_PATH: string = resolve(import.meta.dir, "..", "..");

function detached(argv: string[], opts: { cwd?: string } = {}): void {
  const child = spawn(argv[0]!, argv.slice(1), {
    cwd: opts.cwd,
    stdio: "ignore",
    detached: true,
  });
  child.unref();
}

export function openUrl(url: string): void {
  detached(["open", url]);
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
