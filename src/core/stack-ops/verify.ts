/**
 * Opt-in pre-apply gate for `wt stack apply --verify`.
 *
 * Whole-file slicing kept one property for free: every intermediate slice
 * still compiled, because a changed file moved as a unit. Hunk-level slicing
 * forfeits it — a slice can take a function's body while the import it needs
 * rides a later slice, and nothing notices until that PR's CI goes red. This
 * module restores the guarantee by reconstructing each slice's MATERIALIZED
 * tree (base + the slice's ancestor-closure) into a throwaway worktree and
 * running a configured verify command (`config.stack.verifyCommand`, e.g.
 * `bun run typecheck`) against it BEFORE `applyStack` creates a branch or PR.
 *
 * The tree per slice is exactly what `materializeSliceCommit` commits: the
 * union of files/hunks owned by the slice AND its transitive ancestors
 * (`transitiveAncestors`), nothing from parallel lanes. The worktree is reset
 * to base between slices so a sibling lane's content never leaks into a slice
 * that doesn't descend from it — that's why this isn't a monotonic
 * accumulator: in a forest, "everything emitted so far in topo order" is NOT a
 * slice's parent set.
 *
 * Why a throwaway worktree rather than the holistic one: the trees overwrite
 * working-tree files, and mutating the user's real holistic worktree is
 * destructive. Instead we add a detached worktree at the holistic base and
 * symlink the holistic worktree's installed deps (`verifyDeps`, default
 * `node_modules`) into it, so the command resolves modules without a fresh
 * install.
 *
 * KNOWN LIMITATIONS (a green verify is necessary, not sufficient):
 *  - Deps are symlinked WHOLESALE from the holistic worktree, which has every
 *    dependency the full feature needs. A slice that imports a package the
 *    holistic diff ADDED still resolves it here, even though that slice's own
 *    PR (built before the package.json hunk lands) won't — so a missing-dep
 *    failure can be masked. Likewise `verify_command` (and tsconfig) are read
 *    from each PREFIX tree, so a stack that adds/edits the typecheck script or
 *    tightens tsconfig verifies earlier prefixes under the wrong rules.
 *  - Single-root projects only: the command runs at the worktree ROOT with one
 *    root-level dep symlink, so workspace/monorepo per-package installs aren't
 *    reproduced.
 *  - Single reconstruction branch only: every source-bearing slice must share
 *    one `source`. A re-split is single-source by construction (all sub-slices
 *    carry the original slice's branch), so it verifies against that branch +
 *    its base. A stack mixing two different `source` branches can't be one
 *    base-plus-closure tree and bails to CI.
 * CI remains the authoritative gate; this catches the common breakages early.
 */
import { randomUUID } from "node:crypto";
import { existsSync, rmSync, symlinkSync } from "node:fs";
import { mkdir } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";

import { config } from "../config.ts";
import { gitQuiet, gitRun, revParse } from "../git.ts";
import {
  baseContent,
  DEFAULT_HUNK_CONTEXT,
  fileHunks,
  holisticBase,
  reconstructFile,
} from "./hunks.ts";
import { createLogger } from "../logger.ts";
import { run } from "../proc.ts";
import { topoSortSlices, transitiveAncestors } from "../stack-layout.ts";
import { getStackManifest, isUnsafeSlicePath, type StackSlice } from "../wtstate.ts";
import { listWorktrees } from "../worktree.ts";

const log = createLogger("[stack-verify]");

type Logger = (line: string) => void;

/** One slice's tree-verification result. */
export type PrefixVerify = {
  ordinal: number;
  sliceId: string;
  title: string;
  ok: boolean;
  /** Tail of the command's combined output — populated only on failure. */
  output: string;
};

export type VerifyResult =
  | { ok: true; prefixes: PrefixVerify[] }
  | { ok: false; error: string; prefixes: PrefixVerify[] };

/** Generous ceiling so a wedged verify command can't hang `apply` forever. */
const VERIFY_TIMEOUT_MS = 600_000;

/** Keep failure output readable — the last N lines usually carry the error. */
function tail(text: string, lines = 40): string {
  const all = text.split("\n");
  return all.length <= lines ? text.trimEnd() : all.slice(-lines).join("\n").trimEnd();
}

/** The verify command runs through the user's login shell (pipes/aliases). */
function loginShell(): string {
  return process.env.SHELL || "/bin/bash";
}

/**
 * Reconstruct each slice's materialized tree and run the verify command
 * against it, stopping at the first failure. Pure read + throwaway-worktree
 * work; never mutates the manifest, branches, or the user's worktrees.
 */
export async function verifyStack(stackId: string, onLog: Logger): Promise<VerifyResult> {
  const command = config.stack.verifyCommand;
  if (!command) {
    return {
      ok: false,
      prefixes: [],
      error:
        "apply --verify needs a [stack] verify_command in config.toml (e.g. verify_command = \"bun run typecheck\")",
    };
  }
  const manifest = getStackManifest(stackId);
  if (!manifest) return { ok: false, prefixes: [], error: `no stack manifest: ${stackId}` };
  if (!manifest.holisticBranch) {
    return { ok: false, prefixes: [], error: `manifest ${stackId} has no holisticBranch` };
  }
  const holisticBranch = manifest.holisticBranch;

  // Resolve the SINGLE branch the closure trees reconstruct from. Materialize
  // sources each slice from `slice.source ?? holisticBranch`; the closure
  // reconstruction here uses one branch + one base for the whole stack, so it
  // can faithfully mirror materialize only when every source-bearing slice
  // agrees. A re-split is single-source by construction (its sub-slices all
  // carry the SAME original-slice branch), so it verifies fine — but a stack
  // whose slices source from two DIFFERENT branches can't be reconstructed as
  // one base-plus-closure and bails to CI.
  const sources = new Set(
    manifest.slices.map((s) => s.source).filter((s): s is string => !!s && s !== holisticBranch),
  );
  if (sources.size > 1) {
    return {
      ok: false,
      prefixes: [],
      error: `verify unsupported for mixed-source stacks (slices source from ${[...sources].join(", ")}) — rely on per-PR CI`,
    };
  }
  // The uniform re-split source when present, else the holistic branch. Both
  // the reconstruction branch and the verify worktree base derive from this so
  // the tree matches what `materializeSliceCommit` would commit.
  const reconBranch = sources.size === 1 ? [...sources][0]! : holisticBranch;

  let ordered: StackSlice[];
  try {
    ordered = topoSortSlices(manifest);
  } catch (e) {
    return { ok: false, prefixes: [], error: e instanceof Error ? e.message : String(e) };
  }

  const context = manifest.hunkContext ?? DEFAULT_HUNK_CONTEXT;
  const mainClone = config.paths.mainClone;
  const base = await holisticBase(mainClone, reconBranch);
  if (!(await revParse(base, mainClone))) {
    return { ok: false, prefixes: [], error: `reconstruction base ${base} (of ${reconBranch}) does not resolve` };
  }
  const ancestors = transitiveAncestors(manifest.slices);

  // Deps come from the reconstruction branch's live worktree when it has one
  // (it's where the user installed them) — for a re-split that's the original
  // slice's worktree; for a plain stack it's the holistic worktree — falling
  // back to the holistic worktree, then the main clone. `listWorktrees` parses
  // `git worktree list`; guard it so a parse hiccup degrades to the main clone
  // instead of throwing past the structured-result contract.
  let depsSource = mainClone;
  try {
    const wts = await listWorktrees();
    depsSource =
      wts.find((w) => w.branch === reconBranch)?.path ??
      wts.find((w) => w.branch === holisticBranch)?.path ??
      mainClone;
  } catch (e) {
    onLog(`verify: worktree lookup failed (${e instanceof Error ? e.message : String(e)}); deps from ${mainClone}`);
  }
  // Non-empty dep dirs only — an empty entry would `join(src, "")` to the deps
  // root and symlink the whole tree.
  const deps = config.stack.verifyDeps.filter((d) => d.trim() !== "");

  // pid + a random nonce so a crashed prior run can never alias this path's
  // git registration; prune first so any orphaned `wt-verify-*` admin entry
  // (from a SIGKILL'd run) is reaped before we add.
  const tmp = join(tmpdir(), `wt-verify-${stackId}-${process.pid}-${randomUUID().slice(0, 8)}`);
  rmSync(tmp, { recursive: true, force: true });
  await gitRun(["worktree", "prune"], mainClone);
  const add = await gitRun(["worktree", "add", "--detach", tmp, base], mainClone);
  if (add.exitCode !== 0) {
    return {
      ok: false,
      prefixes: [],
      error: `could not create verify worktree at ${tmp}: ${(add.stderr || add.stdout).trim()}`,
    };
  }

  try {
    // Symlink each dep dir that exists. A missing one isn't fatal (a project
    // may need none); the command failing is the real signal.
    const linked: string[] = [];
    for (const dep of deps) {
      const src = join(depsSource, dep);
      const dst = join(tmp, dep);
      if (existsSync(src) && !existsSync(dst)) {
        try {
          symlinkSync(src, dst);
          linked.push(dep);
        } catch (e) {
          onLog(`  warn: could not symlink ${dep}: ${e instanceof Error ? e.message : String(e)}`);
        }
      }
    }
    onLog(
      linked.length > 0
        ? `verify: deps linked from ${depsSource} (${linked.join(", ")})`
        : `verify: no deps linked (none of ${deps.join(", ") || "(none configured)"} found in ${depsSource})`,
    );

    const prefixes: PrefixVerify[] = [];
    for (const slice of ordered) {
      // The slice's materialized tree = base + (slice ∪ ancestors) content.
      // Reset to base first so a parallel lane's earlier writes don't leak in.
      const reset = await gitRun(["reset", "-q", "--hard", base], tmp);
      if (reset.exitCode !== 0) {
        return { ok: false, prefixes, error: `verify: reset to base failed: ${reset.stderr.trim()}` };
      }
      const clean = await gitRun(
        ["clean", "-fdq", ...deps.flatMap((d) => ["-e", d])],
        tmp,
      );
      if (clean.exitCode !== 0) {
        return { ok: false, prefixes, error: `verify: clean failed: ${clean.stderr.trim()}` };
      }

      const closure = new Set<string>([slice.id, ...(ancestors.get(slice.id) ?? [])]);
      const wholeFiles = new Set<string>();
      const partialOwned = new Map<string, Set<string>>();
      for (const s of manifest.slices) {
        if (!closure.has(s.id)) continue;
        for (const f of s.files) wholeFiles.add(f);
        for (const p of s.partials ?? []) {
          const set = partialOwned.get(p.file) ?? new Set<string>();
          for (const h of p.hunks) set.add(h);
          partialOwned.set(p.file, set);
        }
      }

      const built = await applyClosure(tmp, reconBranch, base, context, wholeFiles, partialOwned);
      if (!built.ok) {
        return {
          ok: false,
          prefixes,
          error: `verify: could not reconstruct the tree for ${slice.id}: ${built.error}`,
        };
      }

      onLog(`verify: ${String(slice.ordinal).padStart(2, "0")} (${slice.id}) → ${command}`);
      const r = await run([loginShell(), "-lc", command], { cwd: tmp, timeoutMs: VERIFY_TIMEOUT_MS });
      const ok = r.exitCode === 0;
      const out = ok ? "" : tail(`${r.stdout}\n${r.stderr}`);
      prefixes.push({ ordinal: slice.ordinal, sliceId: slice.id, title: slice.title, ok, output: out });
      if (!ok) {
        // `run` reports a timeout/kill as exitCode < 0 — distinguish it from a
        // genuine non-zero so the user doesn't read a 10-minute hang as a type error.
        const timedOut = r.exitCode < 0;
        log.warn("verify slice failed", { stackId, slice: slice.id, exitCode: r.exitCode, timedOut });
        // Surface the command's own output — the CLI only prints `error`, so
        // without this the actual tsc errors would never reach the terminal.
        if (out) for (const line of out.split("\n")) onLog(`  │ ${line}`);
        return {
          ok: false,
          prefixes,
          error: timedOut
            ? `verify: ${slice.id} ("${slice.title}") exceeded the ${VERIFY_TIMEOUT_MS / 1000}s timeout (killed)`
            : `verify: ${slice.id} ("${slice.title}") failed \`${command}\` (exit ${r.exitCode})`,
        };
      }
    }
    return { ok: true, prefixes };
  } finally {
    const rm = await gitRun(["worktree", "remove", "--force", tmp], mainClone);
    if (rm.exitCode !== 0) {
      onLog(`  warn: could not remove verify worktree ${tmp}: ${rm.stderr.trim()}`);
      rmSync(tmp, { recursive: true, force: true });
      await gitRun(["worktree", "prune"], mainClone);
    }
  }
}

/**
 * Write one slice's ancestor-closure tree into the (already base-reset) verify
 * worktree: whole files checked out from (or removed per) the reconstruction
 * branch (the holistic branch, or a re-split's single `source` branch),
 * partials reconstructed from their accumulated owned-hunk set. Mirrors
 * `materializeSliceCommit` so verify checks the same content apply commits —
 * including the unknown-id guard, so a manifest whose ids drifted off the
 * branch fails loudly here too rather than reconstructing a stale tree.
 * No commit — the verify command reads the working tree.
 */
async function applyClosure(
  tmp: string,
  reconBranch: string,
  base: string,
  context: number,
  wholeFiles: ReadonlySet<string>,
  partialOwned: ReadonlyMap<string, Set<string>>,
): Promise<{ ok: true } | { ok: false; error: string }> {
  const present: string[] = [];
  const deleted: string[] = [];
  for (const f of wholeFiles) {
    if (await gitQuiet(["cat-file", "-e", `${reconBranch}:${f}`], tmp)) present.push(f);
    else if (await gitQuiet(["cat-file", "-e", `${base}:${f}`], tmp)) deleted.push(f);
    else return { ok: false, error: `file ${f} is on neither ${reconBranch} nor the base` };
  }
  if (present.length > 0) {
    const co = await gitRun(["checkout", reconBranch, "--", ...present], tmp);
    if (co.exitCode !== 0) return { ok: false, error: co.stderr.trim() || "checkout failed" };
  }
  for (const f of deleted) {
    const rm = await gitRun(["rm", "-f", "--", f], tmp);
    if (rm.exitCode !== 0) return { ok: false, error: `git rm ${f}: ${rm.stderr.trim()}` };
  }
  for (const [file, owned] of partialOwned) {
    // Defense in depth: the strict ingest already rejects traversal paths, but
    // this is the one spot that writes a manifest path outside git's pathspec.
    if (isUnsafeSlicePath(file)) return { ok: false, error: `unsafe partial path: ${file}` };
    const fd = await fileHunks(tmp, base, reconBranch, file, context);
    if (fd.binary) return { ok: false, error: `cannot hunk-split binary file ${file}` };
    const known = new Set(fd.hunks.map((h) => h.id));
    const unknown = [...owned].filter((h) => !known.has(h));
    if (unknown.length > 0) {
      return {
        ok: false,
        error: `partial ${file}: hunk id(s) not in the holistic diff: ${unknown.join(", ")} (re-run \`wt stack hunks\`)`,
      };
    }
    const raw = await baseContent(tmp, base, file);
    if (raw.includes("\0")) return { ok: false, error: `cannot hunk-split ${file}: base is not UTF-8 text` };
    const content = reconstructFile(raw, fd.hunks, owned);
    try {
      await mkdir(dirname(join(tmp, file)), { recursive: true });
      await Bun.write(join(tmp, file), content);
    } catch (e) {
      return { ok: false, error: `write ${file}: ${e instanceof Error ? e.message : String(e)}` };
    }
  }
  return { ok: true };
}
