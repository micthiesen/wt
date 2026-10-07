import type { RemoteConfig } from "../core/config.ts";
import type { RemoteWorktreeSummary } from "../core/remote-worktrees.ts";
import type { HarnessId } from "../core/harness/index.ts";

/** In-flight placeholder: hold new inventory rows back until the command completes. */
export type RemoteCreation = {
  remote: RemoteConfig;
  hostKey: string;
  hostLabel: string;
  input: string;
  /** Fleet identities present before this create started. */
  previousKeys: readonly string[];
  status: "creating" | "ready";
  /** One-shot session request, consumed only after successful creation. */
  requestedHarness?: HarnessId;
};

export type RemoteListEntry = RemoteCreation | RemoteWorktreeSummary;

export function isRemoteSummary(
  entry: RemoteListEntry,
): entry is RemoteWorktreeSummary {
  return "slug" in entry;
}

export function remoteEntryKey(entry: RemoteListEntry): string {
  return isRemoteSummary(entry)
    ? `${entry.hostKey}:${entry.slug}`
    : `creating:${entry.hostKey}:${entry.input}`;
}

export function remoteEntryLabel(entry: RemoteListEntry): string {
  return isRemoteSummary(entry) ? entry.slug : entry.input;
}

/** Consume one request only for a completed checkout from this creation. */
export function consumeRemoteCreationSession(
  creation: RemoteCreation,
  row: RemoteWorktreeSummary | undefined,
): HarnessId | undefined {
  if (creation.status !== "ready" || !row || row.hostKey !== creation.hostKey ||
      creation.previousKeys.includes(remoteEntryKey(row))) return undefined;
  const harness = creation.requestedHarness;
  delete creation.requestedHarness;
  return harness;
}

/**
 * Find the authoritative inventory row produced by an in-flight create.
 *
 * The input is not an identity: an issue id can receive a generated suffix,
 * and a title is slugified remotely. Reconcile against the host's inventory
 * delta after the create command finishes.
 */
export function discoveredRemoteCreation(
  creation: RemoteCreation,
  rows: readonly RemoteWorktreeSummary[],
): RemoteWorktreeSummary | undefined {
  const previous = new Set(creation.previousKeys);
  return rows.find(
    (row) =>
      row.hostKey === creation.hostKey &&
      !previous.has(remoteEntryKey(row)),
  );
}

/** Background inventory may see the checkout before installation finishes. */
export function visibleRemoteWorktrees(
  creation: RemoteCreation | null,
  rows: readonly RemoteWorktreeSummary[],
): readonly RemoteWorktreeSummary[] {
  if (!creation) return rows;
  const previous = new Set(creation.previousKeys);
  return rows.filter((row) =>
    row.hostKey !== creation.hostKey || previous.has(remoteEntryKey(row)),
  );
}
