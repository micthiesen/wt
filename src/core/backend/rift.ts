import { existsSync, readdirSync } from "node:fs";
import { basename, dirname, join } from "node:path";

import { git, gitQuiet } from "../git.ts";
import { createLogger } from "../logger.ts";
import { run } from "../proc.ts";
import type {
  BackendCreateInput,
  BackendRemoveInput,
  BackendRemoveResult,
  WorktreeBackend,
} from "./types.ts";

const log = createLogger("[backend:rift]");

/** True when the `rift` binary is on PATH. */
export function riftAvailable(): boolean {
  return Bun.which("rift") !== null;
}

function ensureRiftInstalled(): void {
  if (riftAvailable()) return;
  throw new Error(
    "rift backend selected but `rift` is not on PATH. Install it " +
      "(`npm i -g rift-snapshot`) or set `[backend] kind = \"git-worktree\"` " +
      "in your wt config.",
  );
}

/** A rift-managed checkout carries a `.rift` marker file at its root. */
export function isRiftWorktree(path: string): boolean {
  return existsSync(join(path, ".rift"));
}

/**
 * Rift checkouts are independent clones, so they never appear in
 * `git worktree list`. Discovery scans the worktree root for immediate
 * children carrying a `.rift` marker. Cheap: one readdir + a stat per
 * entry, no subprocess (branch resolution happens in the caller from
 * `.git/HEAD`). Returns absolute paths.
 */
export function listRiftWorktreePaths(worktreeRoot: string): string[] {
  let entries: string[];
  try {
    entries = readdirSync(worktreeRoot, { withFileTypes: true })
      .filter((e) => e.isDirectory())
      .map((e) => e.name);
  } catch {
    // Root doesn't exist yet (no worktrees created) — nothing to scan.
    return [];
  }
  const paths: string[] = [];
  for (const name of entries) {
    const p = join(worktreeRoot, name);
    if (isRiftWorktree(p)) paths.push(p);
  }
  return paths;
}

/**
 * Idempotently register the main clone with rift. `rift init` is a
 * no-op once a `.rift` marker exists, but we guard on the marker first
 * to avoid spawning the binary on the common already-initialized path.
 * Done lazily at first create rather than at TUI startup so wt never
 * pays a rift subprocess just to launch.
 */
async function ensureRiftInit(
  mainClone: string,
  onLog?: (line: string) => void,
): Promise<void> {
  if (isRiftWorktree(mainClone)) return;
  onLog?.("rift init (registering main clone)");
  const r = await run(["rift", "init", "--here"], { cwd: mainClone });
  if (r.exitCode !== 0) {
    // Two first-ever creates can pass the marker check above before either
    // writes it, then race `rift init` — the loser exits non-zero with a
    // UNIQUE-constraint collision. The desired end state (main clone
    // registered) is already reached by the winner, so tolerate it.
    if (isRiftWorktree(mainClone)) return;
    throw new Error(
      `rift init failed: ${(r.stderr || r.stdout || `exit ${r.exitCode}`).trim()}`,
    );
  }
}

/**
 * Switch a freshly-cloned rift checkout onto its target branch. rift
 * copies at a detached HEAD on the main clone's current commit, so:
 *  - existing branch (`baseRef === null`): `git switch` it (DWIM creates a
 *    local tracking branch when it exists only on origin);
 *  - new branch: resolve a start commit that actually exists in this
 *    independent clone. When a sibling worktree OWNS the base (a stacked
 *    parent, `baseSourcePath` set), fetch the AUTHORITATIVE live tip from
 *    it — never trust a same-named branch the CoW clone may have copied
 *    from the main clone, which can be a stale leftover. Only when the
 *    fetch can't run (e.g. a linked git-worktree parent sharing main's db,
 *    which the clone already copied) fall back to the ref in the clone.
 *    With no owning worktree (trunk/origin base), the ref must resolve in
 *    the clone directly.
 */
async function materializeBranch(input: {
  path: string;
  branch: string;
  baseRef: string | null;
  baseSourcePath?: string;
  onLog?: (line: string) => void;
}): Promise<void> {
  const { path, branch, baseRef, baseSourcePath, onLog } = input;
  if (baseRef === null) {
    onLog?.(`switch to ${branch}`);
    await git(["switch", branch], path);
    return;
  }

  const resolvesInClone = (): Promise<boolean> =>
    gitQuiet(["rev-parse", "--verify", "--quiet", `${baseRef}^{commit}`], path);

  let start = baseRef;
  if (baseSourcePath) {
    onLog?.(`fetching base ${baseRef} from ${basename(baseSourcePath)}`);
    const fetched = await run(
      ["git", "fetch", "--no-tags", baseSourcePath, `refs/heads/${baseRef}`],
      { cwd: path },
    );
    if (fetched.exitCode === 0) {
      start = "FETCH_HEAD";
    } else if (await resolvesInClone()) {
      onLog?.(`fetch from ${basename(baseSourcePath)} failed; using base ${baseRef} from the clone`);
      start = baseRef;
    } else {
      throw new Error(
        `could not fetch base ${baseRef} from ${baseSourcePath}: ` +
          `${(fetched.stderr || fetched.stdout || `exit ${fetched.exitCode}`).trim()}`,
      );
    }
  } else if (!(await resolvesInClone())) {
    throw new Error(
      `base ${baseRef} is not in the new checkout and no source ` +
        `worktree was found to fetch it from`,
    );
  }
  onLog?.(`new branch ${branch} off ${baseRef}`);
  await git(["switch", "-c", branch, start], path);
}

/**
 * Copy-on-write checkout via rift. `--copy-all` brings `node_modules`
 * (and other regenerable artifacts rift excludes by default) across for
 * free via the CoW clone, so wt skips its own `pnpm install` for this
 * backend — packages are always present without a fresh install. rift's
 * own `.rift.toml` postcreate hooks (if the repo defines them) still run
 * to sync lockfile state; wt does not pass `--no-hooks`. Note those hooks
 * run during `rift create`, i.e. at the clone-time detached HEAD, BEFORE
 * the branch switch below — a branch-name-sensitive hook sees the main
 * clone's commit, not the target branch (see docs/backends.md).
 */
export const riftBackend: WorktreeBackend = {
  id: "rift",

  async create(input: BackendCreateInput): Promise<void> {
    const { path, branch, slug, baseRef, baseSourcePath, mainClone, onLog } = input;
    ensureRiftInstalled();
    await ensureRiftInit(mainClone, onLog);

    const into = dirname(path);
    const createArgs = ["rift", "create", "--name", slug, "--into", into, "--copy-all"];
    onLog?.(`rift create --copy-all → ${basename(path)}`);
    let created = await run(createArgs, { cwd: mainClone });
    // rift's registry is global and outlives the directory: a checkout
    // deleted out-of-band (a hand `rm -rf`, an aborted create) leaves a
    // record that collides here as "UNIQUE constraint failed: rift.path".
    // The path is already absent (createWorktree guards existsSync), so
    // gc-prune the dangling record and retry once — self-healing rather
    // than making the user run `rift gc` by hand.
    if (
      created.exitCode !== 0 &&
      /UNIQUE constraint|already (registered|exists)/i.test(
        `${created.stderr}${created.stdout}`,
      ) &&
      !existsSync(path)
    ) {
      onLog?.("pruning stale rift registry entry, retrying");
      await run(["rift", "gc"], { cwd: mainClone });
      created = await run(createArgs, { cwd: mainClone });
    }
    if (created.exitCode !== 0) {
      throw new Error(
        `rift create failed: ${(created.stderr || created.stdout || `exit ${created.exitCode}`).trim()}`,
      );
    }
    // rift prints the new workspace path to stdout; `--into <root> --name
    // <slug>` places it at exactly `<root>/<slug>` == `path`, but honor
    // the reported path if rift ever normalizes it differently.
    const reported = created.stdout.trim();
    if (reported && reported !== path) {
      log.warn("rift create path differs from expected", { reported, expected: path });
    }
    if (!existsSync(path)) {
      throw new Error(`rift create did not produce ${path} (got: ${reported})`);
    }

    // The workspace now exists on disk with a `.rift` marker. Materializing
    // the branch is a SEPARATE step that can still fail (bad base, a git
    // error) — and a rift checkout that fails here would linger as a
    // blank-branch ghost row (detached HEAD, discovery keeps surfacing it),
    // unlike git-worktree where a failed `add` registers nothing. So roll
    // the clone back on any failure to keep create atomic.
    try {
      await materializeBranch({ path, branch, baseRef, baseSourcePath, onLog });
    } catch (err) {
      onLog?.(`rolling back partial rift checkout ${basename(path)}`);
      await run(["rift", "remove", path], { cwd: mainClone }).catch(() => {});
      await run(["rift", "gc"], { cwd: mainClone }).catch(() => {});
      throw err;
    }
  },

  async remove(input: BackendRemoveInput): Promise<BackendRemoveResult> {
    const { path, force, mainClone, onLog } = input;
    if (!riftAvailable()) {
      return { ok: false, message: "rift is not on PATH" };
    }
    // Mirror the git-worktree backend's force plumbing: when wt has already
    // cleared its own dirty/unpushed guard (a forced `wt rm`), pass rift's
    // `--force` too. rift trashes a dirty created checkout without it today,
    // but this keeps the two backends symmetric and future-proofs against a
    // rift version that refuses a dirty checkout. Harmless on a clean one.
    const args = force ? ["rift", "remove", "--force", path] : ["rift", "remove", path];
    onLog?.(`rift remove${force ? " --force" : ""} ${basename(path)}`);
    const r = await run(args, { cwd: mainClone });
    if (r.exitCode !== 0 && existsSync(path)) {
      return { ok: false, message: (r.stderr || r.stdout || "rift remove failed").trim() };
    }
    // `rift remove` only trashes the subtree; reclaim the disk now. gc is
    // best-effort — the checkout is already gone from its path either way.
    const gc = await run(["rift", "gc"], { cwd: mainClone });
    if (gc.exitCode !== 0) {
      onLog?.(`rift gc warning: ${(gc.stderr || gc.stdout || "").trim()}`);
    }
    return { ok: true };
  },
};
