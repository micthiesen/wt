import { spawn } from "node:child_process";

export { hideFrontmostAlacritty, openInZed } from "../core/zed.ts";

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
