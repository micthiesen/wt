/**
 * Direct client for Graphite's private CLI API. Speaks the same HTTP
 * surface the `gt` binary uses internally — token loaded from
 * `~/.config/graphite/user_config`, auth as `Authorization: token …`
 * (lowercase scheme; the server rejects `Bearer` and `Token`).
 *
 * The endpoint set, request bodies, and auth scheme were reverse-
 * engineered from `gt`'s captured traffic. Treat the surface as private:
 * Graphite can change shapes or auth at any time. We only call read
 * endpoints here; arming "merge when ready" still shells out to
 * `gt submit -m` (see `armMergeWhenReady` in `core/graphite.ts`) because
 * the submit endpoint expects a full stack-aware payload that the CLI
 * builds for us.
 *
 * Rate limit observed on production: 1500 req per 300s window
 * (~5 req/s sustained). One batched call per refresh tick is well
 * under that budget regardless of worktree count.
 */
import { readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

import { createLogger } from "./logger.ts";

const log = createLogger("[graphite-api]");

const TOKEN_PATH = join(homedir(), ".config", "graphite", "user_config");
const API_BASE = "https://api.graphite.com/v1/graphite";

let _token: string | null | undefined;

function readToken(): string | null {
  if (_token !== undefined) return _token;
  try {
    const raw = readFileSync(TOKEN_PATH, "utf8");
    const parsed = JSON.parse(raw) as { authToken?: unknown };
    _token = typeof parsed.authToken === "string" ? parsed.authToken : null;
  } catch (err) {
    log.warn("could not read graphite token", {
      path: TOKEN_PATH,
      err: err instanceof Error ? err.message : String(err),
    });
    _token = null;
  }
  return _token;
}

/** True when a token is present on disk. Cheap; read once per process. */
export function hasGraphite(): boolean {
  return readToken() !== null;
}

/**
 * Per-PR mergeability state from Graphite. Values observed so far in
 * the wild: `DRAFT`, `NEEDS_REVIEWERS`, `UNRESOLVED_COMMENTS`,
 * `FAILING_REQUIRED`, `RUNNING` (armed, required CI in flight),
 * `QUEUED_TO_MERGE` (armed, awaiting its turn). The set isn't
 * documented and Graphite may add new variants, so we keep it open as
 * `string` and the renderer falls back to a passthrough label for
 * anything unrecognized rather than silently dropping the signal.
 */
export type MergeabilityStatus =
  | "DRAFT"
  | "NEEDS_REVIEWERS"
  | "NEEDS_APPROVAL"
  | "NEEDS_APPROVALS"
  | "UNRESOLVED_COMMENTS"
  | "CHANGES_REQUESTED"
  | "FAILING_REQUIRED"
  | "QUEUED"
  | "QUEUED_TO_MERGE"
  | "RUNNING"
  | "MERGEABLE"
  | (string & {});

export type MergeabilityEntry = {
  prNumber: number;
  status: MergeabilityStatus;
};

async function apiPost<T>(
  route: string,
  body: unknown,
  signal?: AbortSignal,
): Promise<T> {
  const token = readToken();
  if (!token) {
    throw new Error(
      `graphite token not configured (${TOKEN_PATH}); run \`gt auth\``,
    );
  }
  const res = await fetch(`${API_BASE}${route}`, {
    method: "POST",
    headers: {
      Authorization: `token ${token}`,
      "Content-Type": "application/json",
    },
    body: JSON.stringify(body),
    signal,
  });
  if (!res.ok) {
    const text = await res.text().catch(() => "");
    const snippet = text.slice(0, 200);
    throw new Error(
      `graphite ${route} ${res.status}: ${snippet || res.statusText}`,
    );
  }
  return (await res.json()) as T;
}

type MergeabilityResponse = {
  mergeabilityStatuses?: Array<{
    prNumber: number;
    mergeabilityStatus: string;
  }>;
};

/**
 * Per-PR mergeability statuses batched in one round-trip. Returns an
 * empty Map when there are no PRs to query — skips the HTTP call. Keys
 * are PR numbers; missing PRs (e.g. closed, or not visible to the token)
 * simply don't appear in the result.
 */
export async function fetchMergeability(
  args: {
    repoOwner: string;
    repoName: string;
    prNumbers: readonly number[];
  },
  signal?: AbortSignal,
): Promise<Map<number, MergeabilityEntry>> {
  const out = new Map<number, MergeabilityEntry>();
  if (args.prNumbers.length === 0) return out;
  const data = await apiPost<MergeabilityResponse>(
    "/mergeability-status",
    {
      repoOwner: args.repoOwner,
      repoName: args.repoName,
      prNumbers: [...args.prNumbers],
      // The CLI sends its own version; `wt` is honest about being a
      // separate client. Graphite doesn't currently gate on this but
      // the field is required.
      cliVersion: "wt",
    },
    signal,
  );
  for (const entry of data.mergeabilityStatuses ?? []) {
    if (typeof entry.prNumber !== "number") continue;
    if (typeof entry.mergeabilityStatus !== "string") continue;
    out.set(entry.prNumber, {
      prNumber: entry.prNumber,
      status: entry.mergeabilityStatus,
    });
  }
  return out;
}
