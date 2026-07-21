import { existsSync } from "node:fs";

import { git, gitQuiet } from "../git.ts";
import { run } from "../proc.ts";
import type {
  BackendCreateInput,
  BackendRemoveInput,
  BackendRemoveResult,
  WorktreeBackend,
} from "./types.ts";

/**
 * The original mechanism: a linked git worktree sharing the main
 * clone's object db. A new branch is created with `--no-track` (wt owns
 * upstream wiring as an agnostic post-step); an existing branch is
 * checked out as-is.
 */
export const gitWorktreeBackend: WorktreeBackend = {
  id: "git-worktree",

  async create(input: BackendCreateInput): Promise<void> {
    const { path, branch, baseRef, onLog } = input;
    if (baseRef === null) {
      onLog?.(`checkout ${branch}`);
      await git(["worktree", "add", path, branch]);
    } else {
      onLog?.(`new branch ${branch} off ${baseRef}`);
      await git(["worktree", "add", "--no-track", "-b", branch, path, baseRef]);
    }
  },

  async remove(input: BackendRemoveInput): Promise<BackendRemoveResult> {
    const { path, force, mainClone } = input;
    const args = ["worktree", "remove", path];
    if (force) args.push("--force");
    const r = await run(["git", ...args], { cwd: mainClone });
    if (r.exitCode !== 0) {
      // A stale admin entry can block removal; prune, then judge success
      // by whether the checkout is actually gone from disk.
      await gitQuiet(["worktree", "prune"]);
      if (existsSync(path)) {
        return { ok: false, message: (r.stderr || r.stdout || "failed").trim() };
      }
    }
    return { ok: true };
  },
};
