import { existsSync } from "node:fs";
import { resolve as resolvePath } from "node:path";

import { config } from "./config.ts";
import { run, runOk, runQuiet } from "./proc.ts";

export async function git(args: string[], cwd?: string): Promise<string> {
  return runOk(["git", ...args], { cwd: cwd ?? config.paths.mainClone });
}

export async function gitQuiet(args: string[], cwd?: string): Promise<boolean> {
  return runQuiet(["git", ...args], { cwd: cwd ?? config.paths.mainClone });
}

export async function gitRun(
  args: string[],
  cwd?: string,
) {
  return run(["git", ...args], { cwd: cwd ?? config.paths.mainClone });
}

/**
 * Resolve the effective diff/sync base for a worktree, guarding against a
 * dead parent ref. A stacked slice diffs/syncs against its parent branch;
 * once that parent merges and its worktree is cleaned (branch deleted), the
 * recorded base no longer resolves and every `<base>...HEAD` git call errors
 * out (e.g. `git rev-list` via `runOk` throws a raw `fatal: bad revision`).
 * Fall back to trunk so the row degrades to a (fat) trunk diff instead of
 * surfacing that error. `reconcileStack` is the real fix — it reparents the
 * orphan onto trunk in its fork-base record — so this only covers the window before
 * reconcile runs (or if the PR-merged probe hasn't landed yet). An external
 * base (stack-on-stack) still resolves, so it's left untouched.
 */
export async function effectiveBaseOrTrunk(
  wtPath: string,
  effectiveBase?: string | null,
): Promise<string> {
  const trunk = `origin/${config.branch.base}`;
  if (!effectiveBase || effectiveBase === trunk) return trunk;
  return (await revParse(effectiveBase, wtPath)) ? effectiveBase : trunk;
}

/**
 * Is a rebase actually in progress in `cwd`? This is the authoritative test —
 * the presence of git's per-worktree `rebase-merge`/`rebase-apply` state dir —
 * NOT the exit code of `git rebase --abort` (which also fails when there's
 * nothing to abort, the exact ambiguity that produced false "left mid-rebase"
 * reports on slices whose rebase failed at preflight without ever starting).
 */
export async function rebaseInProgress(cwd: string): Promise<boolean> {
  for (const dir of ["rebase-merge", "rebase-apply"]) {
    const r = await gitRun(["rev-parse", "--git-path", dir], cwd);
    const p = r.stdout.trim();
    // `--git-path` is ABSOLUTE for a linked worktree (the common case here) and
    // relative to `cwd` only for the main clone. `resolvePath(cwd, p)` is
    // correct for both — Node's `resolve` returns an absolute second arg
    // unchanged and joins a relative one onto `cwd`. Don't "simplify" this.
    if (p && existsSync(resolvePath(cwd, p))) return true;
  }
  return false;
}

export type MergeConflictProbe =
  | { status: "clean"; base: string }
  | { status: "conflict"; base: string; files: readonly string[] }
  /** The worktree is mid-rebase (conflict being resolved by hand or by
   *  `/restack`) — HEAD is transient, so the merge dry-run is skipped. */
  | { status: "rebasing"; base: string }
  | { status: "unknown"; base: string };

/**
 * Dry-run merge of `headRef` against `base` via `git merge-tree
 * --write-tree` — a real 3-way merge in the object database that never
 * touches a working tree or index. Approximates "will `headRef` rebase
 * cleanly onto `base`": exit 0 = clean, exit 1 = conflict, anything else
 * = unknown (rendered without any glyph).
 *
 * The exit-1 case is overloaded: git returns it BOTH for a real conflict
 * AND for an unresolvable ref ("not something we can merge", which it
 * prints to stderr with an empty stdout). A genuine conflict always
 * writes the result tree OID to stdout first, so non-empty stdout is
 * what tells the two apart — a bare exit code would false-positive a
 * conflict glyph onto any worktree whose base ref has gone missing.
 *
 * It's a merge, not a rebase replay, so for a multi-commit branch it's a
 * strong hint rather than a guarantee — good enough to warn before a
 * restack, cheap enough to run per row.
 */
export async function mergeConflictProbe(
  headRef: string,
  base: string,
  cwd?: string,
): Promise<MergeConflictProbe> {
  // Mid-rebase, HEAD is a moving target (detached on the pick sequence)
  // and the interesting fact is the rebase itself — report it instead of
  // probing a transient tree. The TUI renders this as "resolution in
  // progress" rather than a conflict warning.
  if (cwd && (await rebaseInProgress(cwd))) {
    return { status: "rebasing", base };
  }
  const r = await gitRun(
    ["merge-tree", "--write-tree", "--name-only", "--no-messages", base, headRef],
    cwd,
  );
  if (r.exitCode === 0) return { status: "clean", base };
  if (r.exitCode === 1 && r.stdout.trim()) {
    // stdout: "<tree-oid>\n<file>\n<file>…" — first line is the result
    // tree OID, the rest are the conflicting paths.
    const files = r.stdout
      .split("\n")
      .map((l) => l.trim())
      .filter(Boolean)
      .slice(1);
    return { status: "conflict", base, files };
  }
  return { status: "unknown", base };
}

/**
 * Resolve a ref to its commit SHA in `cwd` (default: the main clone),
 * or null when it doesn't resolve. The one canonical rev-parse helper —
 * the engine, stack ops, and base resolution all share it.
 */
export async function revParse(ref: string, cwd?: string): Promise<string | null> {
  const r = await gitRun(["rev-parse", "--verify", "--quiet", `${ref}^{commit}`], cwd);
  const sha = r.stdout.trim();
  return r.exitCode === 0 && sha ? sha : null;
}

/** First ref among `refs` that resolves to a commit in `cwd`, as a SHA. */
export async function firstSha(cwd: string, refs: string[]): Promise<string | null> {
  for (const ref of refs) {
    const sha = await revParse(ref, cwd);
    if (sha) return sha;
  }
  return null;
}

/** Does `branch` exist as a local head? */
export async function localBranchExists(branch: string, cwd?: string): Promise<boolean> {
  return gitQuiet(["show-ref", "--verify", "--quiet", `refs/heads/${branch}`], cwd);
}

/** Does `branch` exist as an origin remote-tracking ref? */
export async function originBranchExists(branch: string, cwd?: string): Promise<boolean> {
  return gitQuiet(["show-ref", "--verify", "--quiet", `refs/remotes/origin/${branch}`], cwd);
}

export async function branchExists(branch: string): Promise<boolean> {
  return (await localBranchExists(branch)) || originBranchExists(branch);
}

/**
 * `branch` itself when the local head exists, else `origin/<branch>` —
 * a ref other git commands can resolve either way. Doesn't verify the
 * origin ref; pair with `branchExists` when absence is an error.
 */
export async function localOrOriginRef(branch: string): Promise<string> {
  return (await localBranchExists(branch)) ? branch : `origin/${branch}`;
}

export async function branchIsGone(branch: string): Promise<boolean> {
  const r = await run(
    ["git", "for-each-ref", "--format=%(upstream:track)", `refs/heads/${branch}`],
    { cwd: config.paths.mainClone },
  );
  if (r.exitCode !== 0) return false;
  return r.stdout.trim() === "[gone]";
}

let _mainFirstParents: Promise<Set<string>> | null = null;

/**
 * SHAs on origin/main's first-parent chain. A branch tip that lives
 * here is just an older main commit (nothing was merged *from* the
 * branch); one that sits off this chain was pulled in via a real merge
 * commit.
 *
 * Cached as a promise (not a value) so concurrent callers on a cold
 * cache share a single `git rev-list` — the queryFn for every non-main
 * worktree's `branchIsMerged` calls this, and they all fire at once
 * after `invalidateMainFirstParents()`.
 */
export function mainFirstParentShas(): Promise<Set<string>> {
  if (_mainFirstParents) return _mainFirstParents;
  _mainFirstParents = (async () => {
    const r = await run(
      ["git", "rev-list", "--first-parent", `origin/${config.branch.base}`],
      { cwd: config.paths.mainClone },
    );
    return new Set(
      r.exitCode === 0 ? r.stdout.split("\n").filter(Boolean) : [],
    );
  })();
  return _mainFirstParents;
}

/** Invalidate cached first-parent set after a fetch. */
export function invalidateMainFirstParents(): void {
  _mainFirstParents = null;
}

/**
 * Subject line of the *oldest* commit on the branch since `origin/main`.
 * That's the human's "what is this work" framing — captures intent
 * before a PR exists. Returns null if the branch has no commits ahead
 * of base, or `git log` fails.
 */
export async function firstCommitSubject(wtPath: string): Promise<string | null> {
  const r = await run(
    [
      "git",
      "log",
      "--reverse",
      "--format=%s",
      `origin/${config.branch.base}..HEAD`,
    ],
    { cwd: wtPath, timeoutMs: 5_000 },
  );
  if (r.exitCode !== 0) return null;
  const first = r.stdout.split("\n").find((l) => l.length > 0);
  return first ?? null;
}

export async function branchIsMerged(branch: string): Promise<boolean> {
  // Real-divergence gate; FF-aligned branches skip out below.
  if (
    !(await gitQuiet([
      "merge-base",
      "--is-ancestor",
      branch,
      `origin/${config.branch.base}`,
    ]))
  ) {
    return false;
  }
  let branchSha: string;
  let mainSha: string;
  try {
    branchSha = await git(["rev-parse", "--verify", branch]);
    mainSha = await git(["rev-parse", "--verify", `origin/${config.branch.base}`]);
  } catch {
    return false;
  }
  if (branchSha === mainSha) return false;
  // Branch tip on main's first-parent chain = just an older main SHA
  // (branch never got its own commits). Real merge-commit merges attach
  // the branch via a second parent.
  const fps = await mainFirstParentShas();
  return !fps.has(branchSha);
}
