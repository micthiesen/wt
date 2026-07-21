import type { RemoteConfig } from "./config.ts";

/**
 * Encode argv into one shell-safe token. OpenSSH sends its remote command
 * through the account's login shell (Fish on CachyOS), so forwarding raw argv
 * would make quoting shell-dependent. Base64url keeps the transport alphabet
 * to letters, digits, `_`, and `-`; the remote `_remote` entrypoint restores
 * the exact string array before normal CLI dispatch.
 */
export function encodeRemoteArgs(argv: readonly string[]): string {
  return Buffer.from(JSON.stringify(argv), "utf8").toString("base64url");
}

export function decodeRemoteArgs(payload: string): string[] {
  let value: unknown;
  try {
    value = JSON.parse(Buffer.from(payload, "base64url").toString("utf8"));
  } catch {
    throw new Error("invalid remote argv payload");
  }
  if (!Array.isArray(value) || !value.every((arg) => typeof arg === "string")) {
    throw new Error("remote argv payload must be an array of strings");
  }
  return value;
}

/** Quote the configured executable path; argv itself travels encoded. */
function remoteExecutable(path: string): string {
  if (path === "~") return '"$HOME"';
  if (path.startsWith("~/")) {
    const suffix = path.slice(2).replaceAll("'", "'\\''");
    return `"$HOME"/'${suffix}'`;
  }
  return `'${path.replaceAll("'", "'\\''")}'`;
}

/** Command string handed to SSH for either the remote TUI or encoded CLI. */
export function remoteWtCommand(
  remote: RemoteConfig,
  argv: readonly string[] | null,
): string {
  const executable = remoteExecutable(remote.wtPath);
  return argv === null
    ? `exec ${executable}`
    : `exec ${executable} _remote ${encodeRemoteArgs(argv)}`;
}
