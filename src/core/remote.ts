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
        remote.host,
        remoteWtCommand(remote, argv.length > 0 ? argv : null),
      ],
      { stdin: "inherit", stdout: "inherit", stderr: "inherit" },
    );
    return proc.exited;
  }

  return runStreaming(
    ["ssh", remote.host, remoteWtCommand(remote, argv)],
    { cwd: process.cwd(), onLine: opts.onLine },
  );
}
