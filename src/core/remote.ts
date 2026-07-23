import type { RemoteConfig } from "./config.ts";
import { runStreaming } from "./proc.ts";
import { remoteWtCommand } from "./remote-protocol.ts";

export type RemoteRunOptions = {
  /** Allocate a PTY and inherit stdio for the remote wt TUI. */
  interactive?: boolean;
  /** Receives sanitized stdout/stderr lines for non-interactive commands. */
  onLine?: (line: string) => void;
};

/**
 * Argv for an interactive (PTY-allocating) remote wt invocation. Shared
 * by the classic full-screen handoff (`runRemoteWt` interactive) and the
 * hub's local tmux wrapper sessions, so both ride the same timeouts and
 * transport encoding. Bound the TCP connect + detect a mid-session drop:
 * without these, an unreachable host hangs on the OS connect timeout
 * (often 60s+) — for classic that's AFTER the renderer is already
 * suspended and the terminal handed off, a frozen blank screen. BatchMode
 * is deliberately omitted so interactive password/2FA auth still works.
 */
export function interactiveRemoteSshArgv(
  remote: RemoteConfig,
  argv: readonly string[],
): string[] {
  return [
    "ssh",
    "-t",
    "-o",
    "ConnectTimeout=10",
    "-o",
    "ServerAliveInterval=5",
    "-o",
    "ServerAliveCountMax=3",
    remote.host,
    remoteWtCommand(remote, argv.length > 0 ? argv : null),
  ];
}

/** Run this target's wt over ordinary SSH, relying on ~/.ssh/config. */
export async function runRemoteWt(
  remote: RemoteConfig,
  argv: readonly string[],
  opts: RemoteRunOptions = {},
): Promise<number> {
  if (opts.interactive) {
    const proc = Bun.spawn(interactiveRemoteSshArgv(remote, argv), {
      stdin: "inherit",
      stdout: "inherit",
      stderr: "inherit",
    });
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
