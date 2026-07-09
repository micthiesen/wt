import { readWtState, withWtStateLock, writeWtState } from "./io.ts";
import type { StackManifest, StackSlice } from "./types.ts";

// ---------- Stack manifests ----------

/** Every stored stack manifest, in `stackId` insertion order. */
export function listStackManifests(): StackManifest[] {
  return Object.values(readWtState().stacks);
}

/** One manifest by id, or `null` when absent. */
export function getStackManifest(stackId: string): StackManifest | null {
  return readWtState().stacks[stackId] ?? null;
}

/**
 * The stackId of the manifest that owns `branch` — matching a slice
 * branch OR the holistic origin branch — or `null` when none does. Lets
 * stack subcommands resolve their target from the current worktree's
 * branch instead of making the caller pass an id they'd have to look up
 * (the id is lower-kebab `eng-1234`, not the branch's `michael/eng-1234-…`).
 * Returns the first match; branches are unique across manifests in
 * practice, so ambiguity isn't a real case.
 */
export function findStackIdByBranch(branch: string): string | null {
  if (!branch) return null;
  for (const m of Object.values(readWtState().stacks)) {
    if (m.holisticBranch === branch) return m.stackId;
    if (m.slices.some((s) => s.branch === branch)) return m.stackId;
  }
  return null;
}

/** Insert or replace a manifest wholesale. Keyed by `manifest.stackId`. */
export function putStackManifest(manifest: StackManifest): void {
  withWtStateLock(() => {
    const state = readWtState();
    writeWtState({
      ...state,
      stacks: { ...state.stacks, [manifest.stackId]: manifest },
    });
  });
}

/**
 * Shallow-merge a partial onto an existing manifest. No-op (returns
 * false) when the manifest is absent. `slices` is replaced wholesale
 * when present in the patch — use `updateStackSlice` for targeted edits.
 */
export function patchStackManifest(
  stackId: string,
  patch: Partial<StackManifest>,
): boolean {
  return withWtStateLock(() => {
    const state = readWtState();
    const prev = state.stacks[stackId];
    if (!prev) return false;
    writeWtState({
      ...state,
      stacks: { ...state.stacks, [stackId]: { ...prev, ...patch } },
    });
    return true;
  });
}

/**
 * Patch a single slice within a manifest (e.g. record its `pr` and flip
 * `status` to "open" after materialization). No-op (false) when the
 * manifest or slice is absent.
 */
export function updateStackSlice(
  stackId: string,
  sliceId: string,
  patch: Partial<StackSlice>,
): boolean {
  return withWtStateLock(() => {
    const state = readWtState();
    const prev = state.stacks[stackId];
    if (!prev) return false;
    let hit = false;
    const slices = prev.slices.map((s) => {
      if (s.id !== sliceId) return s;
      hit = true;
      return { ...s, ...patch };
    });
    if (!hit) return false;
    writeWtState({
      ...state,
      stacks: { ...state.stacks, [stackId]: { ...prev, slices } },
    });
    return true;
  });
}
