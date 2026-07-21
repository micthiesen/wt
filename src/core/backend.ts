/**
 * Flat barrel for the worktree-backend seam. Importers use this path
 * (`./backend.ts`), never the files under `backend/` directly.
 */
import { gitWorktreeBackend } from "./backend/git.ts";
import { isRiftWorktree, riftBackend } from "./backend/rift.ts";
import type { BackendKind, WorktreeBackend } from "./backend/types.ts";

export type {
  BackendKind,
  BackendCheckout,
  BackendCreateInput,
  BackendRemoveInput,
  BackendRemoveResult,
  WorktreeBackend,
} from "./backend/types.ts";
export { isRiftWorktree, listRiftWorktreePaths, riftAvailable } from "./backend/rift.ts";

const BY_KIND: Record<BackendKind, WorktreeBackend> = {
  "git-worktree": gitWorktreeBackend,
  rift: riftBackend,
};

/** The backend a config selects for NEW checkouts. */
export function getBackend(kind: BackendKind): WorktreeBackend {
  return BY_KIND[kind];
}

/**
 * The backend that OWNS an existing checkout, derived from its on-disk
 * shape (a `.rift` marker → rift, otherwise a git worktree). Removal
 * dispatches through this so a checkout created under one backend is
 * still torn down correctly after the config's `kind` is flipped — the
 * axis is detected, not stored.
 */
export function getBackendForPath(path: string): WorktreeBackend {
  return isRiftWorktree(path) ? riftBackend : gitWorktreeBackend;
}
