import type { RemoteConfig } from "./config.ts";
import { runStreaming } from "./proc.ts";
import { remoteWtCommand } from "./remote-protocol.ts";

export type RemoteRunOptions = {
  /** Allocate a PTY and inherit stdio for the remote wt TUI. */
  interactive?: boolean;
  /** Receives sanitized stdout/stderr lines for non-interactive commands. */
  onLine?: (line: string) => void;
};

/** Run this target's wt over ordinary SSH, relying on ~/.ssh/config. */
export async function runRemoteWt(
  remote: RemoteConfig,
  argv: readonly string[],
  opts: RemoteRunOptions = {},
): Promise<number> {
  if (opts.interactive) {
    const proc = Bun.spawn(
      [
        "ssh",
        "-t",
        // Bound the TCP connect + detect a mid-session drop. Without
        // these, an unreachable host hangs on the OS connect timeout
        // (often 60s+) AFTER the renderer is already suspended and the
        // terminal handed off — a frozen blank screen. BatchMode is
        // deliberately omitted here so interactive password/2FA auth
        // still works; only the timeouts are added.
        "-o",
        "ConnectTimeout=10",
        "-o",
        "ServerAliveInterval=5",
        "-o",
        "ServerAliveCountMax=3",
        remote.host,
        remoteWtCommand(remote, argv.length > 0 ? argv : null),
      ],
      { stdin: "inherit", stdout: "inherit", stderr: "inherit" },
    );
    return proc.exited;
  }

  return runStreaming(
    [
      "ssh",
      "-o",
      "BatchMode=yes",
      "-o",
      "ConnectTimeout=5",
      "-o",
      "ServerAliveInterval=5",
      "-o",
      "ServerAliveCountMax=3",
      remote.host,
      remoteWtCommand(remote, argv),
    ],
    { cwd: process.cwd(), onLine: opts.onLine },
  );
}
