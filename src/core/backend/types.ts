/**
 * Worktree backends: the seam that decides HOW an isolated branch
 * checkout is materialized on disk. Deliberately narrow — create and
 * remove are the only two filesystem mutation points (see
 * `lifecycle.ts`); everything else wt does to a worktree (fork-base
 * recording, .env copy, stage pin, upstream, locks, status) is
 * backend-agnostic and stays in `lifecycle.ts` / `worktree.ts`.
 *
 * Two built-ins today:
 *  - `git-worktree` — `git worktree add/remove`. One shared object db,
 *    branches visible in the main clone, discovered via `git worktree
 *    list --porcelain`.
 *  - `rift` — copy-on-write clone (`rift create/remove`). Each checkout
 *    is an INDEPENDENT git repo (its own `.git`, detached HEAD, then a
 *    branch switched in the copy). Not visible to `git worktree list`,
 *    so discovery scans the worktree root for `.rift` markers instead.
 *
 * The backend axis is orthogonal to the (in-flight) remote axis: remote
 * chooses WHERE a worktree lives (this machine vs an SSH host running
 * its own wt); backend chooses how it's materialized LOCALLY. A remote
 * host picks its own backend independently.
 */

export type BackendKind = "git-worktree" | "rift";

export type BackendCreateInput = {
  /** Target checkout path — always `<worktreeRoot>/<slug>`. */
  path: string;
  /** Branch the checkout must end up on. */
  branch: string;
  slug: string;
  /**
   * Base ref for a NEW branch (`origin/main`, a parent branch, …).
   * `null` when `branch` already exists — the backend just checks it out.
   */
  baseRef: string | null;
  /**
   * The on-disk worktree that OWNS `baseRef` when it's a sibling branch
   * (a stacked parent), or undefined for a trunk/origin base. Only the
   * `rift` backend needs it: rift checkouts are independent clones, so a
   * stacked parent's commits live in that parent's `.git`, not in the
   * main clone the child is cloned from — rift fetches the base from
   * here. The git-worktree backend shares one object db and ignores it.
   */
  baseSourcePath?: string;
  mainClone: string;
  onLog?: (line: string) => void;
};

export type BackendRemoveInput = {
  path: string;
  slug: string;
  force: boolean;
  mainClone: string;
  onLog?: (line: string) => void;
};

export type BackendRemoveResult = {
  ok: boolean;
  /** Failure detail when `ok` is false; the checkout is left on disk. */
  message?: string;
};

/** A checkout discovered on disk during listing. */
export type BackendCheckout = {
  path: string;
  branch: string;
};

export interface WorktreeBackend {
  readonly id: BackendKind;
  /**
   * Materialize a checkout at `input.path` sitting on `input.branch`.
   * Branch creation/switch is the backend's job; wt handles everything
   * around it. Throws on failure (the caller's lock `finally` releases).
   */
  create(input: BackendCreateInput): Promise<void>;
  /**
   * Tear the checkout down. Returns `{ ok: false, message }` (rather
   * than throwing) when the checkout could not be removed and is still
   * on disk, matching the `git worktree remove` fallback contract.
   */
  remove(input: BackendRemoveInput): Promise<BackendRemoveResult>;
}
