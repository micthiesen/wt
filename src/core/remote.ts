import { Data, Effect } from "effect";

import type { RemoteConfig } from "./config.ts";
import { causeMessage } from "./errors.ts";
import { runStreaming, terminateSubprocess } from "./proc.ts";
import { remoteWtCommand } from "./remote-protocol.ts";
import { ensureRemoteRuntime } from "./remote-runtime.ts";

export type RemoteRunOptions = {
  /** Allocate a PTY and inherit stdio for the remote wt TUI. */
  interactive?: boolean;
  /** Receives sanitized stdout/stderr lines for non-interactive commands. */
  onLine?: (line: string) => void;
};

/**
 * Non-interactive SSH argv for one encoded remote wt invocation.
 * Exported for long-running callers (notably the action registry), which
 * need to supervise the SSH process themselves instead of awaiting the
 * captured-output `runRemoteWt` wrapper.
 */
export function remoteWtSshArgv(
  remote: RemoteConfig,
  argv: readonly string[],
): string[] {
  return [
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
  ];
}

function shellQuote(value: string): string {
  return `'${value.replaceAll("'", "'\\''")}'`;
}

/**
 * Run an ordinary process in a remote checkout without depending on the
 * remote host's wt version. The account login shell only sees a fixed
 * `/bin/sh -c` wrapper; cwd and argv are quoted for that wrapper.
 */
export function remoteProcessSshArgv(
  remote: RemoteConfig,
  cwd: string,
  argv: readonly string[],
): string[] {
  if (argv.length === 0) throw new Error("remote process argv is empty");
  const script = `cd ${shellQuote(cwd)} && exec ${argv.map(shellQuote).join(" ")}`;
  return [
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
    `exec /bin/sh -c ${shellQuote(script)}`,
  ];
}

/**
 * Argv for an interactive (PTY-allocating) remote wt invocation — the
 * full-screen handoff (`runRemoteWt` interactive). Bound the TCP
 * connect + detect a mid-session drop:
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
export class RemoteRunError extends Data.TaggedError("RemoteRunError")<{
  readonly operation: "spawn" | "wait";
  readonly cause: unknown;
}> {
  override get message(): string {
    return `${this.operation}: ${causeMessage(this.cause)}`;
  }
}

export function runRemoteWt(
  remote: RemoteConfig,
  argv: readonly string[],
  opts: RemoteRunOptions = {},
): Effect.Effect<number, RemoteRunError> {
  return ensureRemoteRuntime(remote, opts.onLine).pipe(
    Effect.mapError((cause) => new RemoteRunError({ operation: "spawn", cause })),
    Effect.flatMap(() => runPreparedRemoteWt(remote, argv, opts)),
  );
}

function runPreparedRemoteWt(
  remote: RemoteConfig,
  argv: readonly string[],
  opts: RemoteRunOptions,
): Effect.Effect<number, RemoteRunError> {
  if (opts.interactive) {
    return Effect.acquireUseRelease(
      Effect.try({
        try: () => Bun.spawn(interactiveRemoteSshArgv(remote, argv), {
          stdin: "inherit",
          stdout: "inherit",
          stderr: "inherit",
        }),
        catch: (cause) => new RemoteRunError({ operation: "spawn", cause }),
      }),
      (proc) => Effect.tryPromise({
        try: () => proc.exited,
        catch: (cause) => new RemoteRunError({ operation: "wait", cause }),
      }),
      (proc) => terminateSubprocess(proc),
    );
  }

  return runStreaming(
    remoteWtSshArgv(remote, argv),
    { cwd: process.cwd(), onLine: opts.onLine },
  ).pipe(
    Effect.mapError((cause) => new RemoteRunError({ operation: "wait", cause })),
  );
}
