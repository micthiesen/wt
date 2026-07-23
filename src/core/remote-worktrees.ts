import type { RemoteConfig } from "./config.ts";
import { run } from "./proc.ts";
import { remoteWtCommand } from "./remote-protocol.ts";
import { StatusKind, type StatusKind as StatusKindValue } from "./types.ts";

export type RemoteWorktreeSummary = {
  hostLabel: string;
  slug: string;
  branch: string;
  path: string;
  stage: string;
  exists: boolean;
  status: StatusKindValue;
  statusLabel: string;
  statusAge: string | null;
  statusOp: string | null;
  dirty: boolean;
  unpushed: number;
  linearUrl: string | null;
};

const STATUS_KINDS = new Set<string>(Object.values(StatusKind));

/**
 * The remote `wt ls --json` runs through the account's login shell, which
 * can prepend/append stray output (fish/bash startup banners, direnv,
 * asdf/nvm, motd tooling) even for a non-interactive command. That noise
 * corrupts a naive `JSON.parse`, and the resulting `SyntaxError` is
 * indistinguishable in the UI from a genuine SSH failure. Only the argv-IN
 * direction is base64-hardened; this hardens the JSON-OUT direction: parse
 * the raw text, and on failure fall back to the outer `[...]` slice (the
 * payload is pretty-printed and is the last real thing wt emits) before
 * giving up with a diagnostic that names the actual cause.
 */
function parseWorktreeJson(raw: string): unknown {
  try {
    return JSON.parse(raw);
  } catch {
    const start = raw.indexOf("[");
    const end = raw.lastIndexOf("]");
    if (start >= 0 && end > start) {
      try {
        return JSON.parse(raw.slice(start, end + 1));
      } catch {
        /* fall through to the diagnostic below */
      }
    }
    const snippet = raw.trim().slice(0, 200);
    throw new Error(
      `remote wt ls did not return JSON — check the remote shell startup for stray output. Got: ${snippet || "(empty)"}`,
    );
  }
}

export function parseRemoteWorktrees(
  raw: string,
  hostLabel: string,
): RemoteWorktreeSummary[] {
  const value: unknown = parseWorktreeJson(raw);
  if (!Array.isArray(value)) throw new Error("remote wt ls returned non-array JSON");
  return value.map((entry, index) => {
    if (!entry || typeof entry !== "object") {
      throw new Error(`remote worktree ${index} is not an object`);
    }
    const row = entry as Record<string, unknown>;
    const str = (key: string): string => {
      const v = row[key];
      if (typeof v !== "string") throw new Error(`remote worktree ${index}.${key} is not a string`);
      return v;
    };
    const status = str("status");
    if (!STATUS_KINDS.has(status)) {
      throw new Error(`remote worktree ${index}.status is invalid: ${status}`);
    }
    const statusLabel = str("status_label");
    const statusOp = typeof row.status_op === "string"
      ? row.status_op
      : status === StatusKind.Busy &&
          (statusLabel === "init" || statusLabel.startsWith("init:"))
        ? "init"
        : null;
    return {
      hostLabel,
      slug: str("slug"),
      branch: str("branch"),
      path: str("path"),
      stage: str("stage"),
      exists: row.exists === true,
      status: status as StatusKindValue,
      statusLabel,
      statusAge: typeof row.status_age === "string" ? row.status_age : null,
      statusOp,
      dirty: row.dirty === true,
      unpushed:
        typeof row.unpushed === "number" &&
        Number.isInteger(row.unpushed) &&
        row.unpushed >= 0
          ? row.unpushed
          : 0,
      linearUrl: typeof row.linear_url === "string" ? row.linear_url : null,
    };
  });
}

/** Read the authoritative worktree list from one configured SSH host. */
export async function fetchRemoteWorktrees(
  remote: RemoteConfig,
  signal?: AbortSignal,
): Promise<RemoteWorktreeSummary[]> {
  const result = await run(
    [
      "ssh",
      "-o",
      "BatchMode=yes",
      "-o",
      "ConnectTimeout=5",
      remote.host,
      remoteWtCommand(remote, ["ls", "--json"]),
    ],
    { cwd: process.cwd(), timeoutMs: 15_000, signal },
  );
  if (result.exitCode !== 0) {
    throw new Error(result.stderr.trim() || result.stdout.trim() || `SSH exited ${result.exitCode}`);
  }
  return parseRemoteWorktrees(result.stdout, remote.label);
}
