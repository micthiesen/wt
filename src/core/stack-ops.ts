/**
 * Materialization + maintenance for stack manifests. wt owns the
 * manifest (truth); this module turns a planned manifest into real
 * worktrees, commits, and draft PRs (`applyStack`), reports the manifest
 * DAG against live reality (`stackStatus`), and drives the squash-safe
 * engine for restack/land (`rebaseStack`). The genuinely hard part
 * (anchor + cherry-pick replay) is delegated to `RestackEngine`; this
 * module never touches `.git/stack` directly.
 */
import { config } from "./config.ts";
import { gitQuiet, gitRun } from "./git.ts";
import { createWorktree } from "./lifecycle.ts";
import { createLogger } from "./logger.ts";
import {
  createDraftPr,
  viewPrInfo,
  type LivePrInfo,
} from "./github.ts";
import { restackEngine, type EngineResult } from "./restack-engine.ts";
import {
  isLaneRoot,
  resolveParentBranch,
  topoSortSlices,
} from "./stack-layout.ts";
import {
  getStackManifest,
  patchStackManifest,
  updateStackSlice,
  type StackManifest,
  type StackSlice,
} from "./wtstate.ts";

const log = createLogger("[stack-ops]");

export type Logger = (line: string) => void;

/**
 * Reproduce a slice's content as a single commit in its fresh worktree.
 * Slices are a file-level partition of the holistic diff, so checking
 * out each slice's files from the holistic branch on top of the parent
 * reproduces the holistic tree exactly across the chain.
 *
 * Edge cases the holistic checkout doesn't cover on its own:
 *  - files the holistic diff DELETES aren't on `holisticBranch`, so
 *    `checkout --` can't remove them — `git rm` handles those.
 *  - a file in the slice list that's neither on the holistic branch nor
 *    in the worktree is unreproducible and surfaces as an error.
 *
 * Rename contract: a rename `a -> b` on the holistic branch must list
 * BOTH paths in `slice.files` (old + new) so `a` is removed and `b`
 * checked out. The planner (`/split`) owns that; this only reproduces.
 */
async function materializeSliceCommit(
  wtPath: string,
  holisticBranch: string,
  slice: StackSlice,
  onLog: Logger,
): Promise<{ ok: true } | { ok: false; error: string }> {
  const present: string[] = [];
  const deleted: string[] = [];
  const missing: string[] = [];
  for (const f of slice.files) {
    if (await gitQuiet(["cat-file", "-e", `${holisticBranch}:${f}`], wtPath)) {
      present.push(f);
    } else if (await gitQuiet(["cat-file", "-e", `HEAD:${f}`], wtPath)) {
      // Absent on holistic but present in the parent tree → a deletion.
      deleted.push(f);
    } else {
      missing.push(f);
    }
  }
  if (missing.length > 0) {
    return {
      ok: false,
      error: `files not on ${holisticBranch} and not in worktree: ${missing.join(", ")}`,
    };
  }
  if (present.length > 0) {
    const r = await gitRun(["checkout", holisticBranch, "--", ...present], wtPath);
    if (r.exitCode !== 0) {
      return { ok: false, error: r.stderr.trim() || "checkout failed" };
    }
    onLog(`  checked out ${present.length} file(s) from ${holisticBranch}`);
  }
  for (const f of deleted) {
    const r = await gitRun(["rm", "-f", "--", f], wtPath);
    if (r.exitCode !== 0) {
      return { ok: false, error: `git rm ${f}: ${r.stderr.trim()}` };
    }
  }
  if (deleted.length > 0) onLog(`  removed ${deleted.length} deleted file(s)`);
  // `checkout --` and `git rm` already stage every slice path, so no
  // extra `git add` is needed — and `add -A` would risk staging an
  // unrelated untracked file that happens to match a slice path.
  // An empty staging area means the slice adds nothing on top of its
  // parent (a mis-partitioned manifest); surface that clearly instead of
  // letting `git commit` fail with a generic status dump.
  if (await gitQuiet(["diff", "--cached", "--quiet"], wtPath)) {
    return { ok: false, error: `slice ${slice.id} produced no changes vs its parent` };
  }
  const commit = await gitRun(["commit", "-m", slice.title], wtPath);
  if (commit.exitCode !== 0) {
    return {
      ok: false,
      error: (commit.stderr || commit.stdout).trim() || "commit failed",
    };
  }
  return { ok: true };
}

/** A minimal-but-valid PR body. Richer bodies are authored by a skill later. */
function sliceBody(manifest: StackManifest, slice: StackSlice): string {
  const lines = [
    `Part ${slice.ordinal} of the ${manifest.issue} stack.`,
    "",
    `Stacked on \`${resolveParentBranch(manifest, slice)}\`.`,
  ];
  if (slice.oversized && slice.oversizedReason) {
    lines.push("", `> Oversized (sanctioned): ${slice.oversizedReason}`);
  }
  return lines.join("\n");
}

export type ApplyOptions = {
  /** Run `pnpm install` per slice. Default false — slow; install where needed. */
  install?: boolean;
};

export type ApplyResult = {
  /** Slices newly materialized this run. */
  materialized: string[];
  /** Slices skipped because they were already open/merged. */
  skipped: string[];
  /** First fatal error, if any — apply stops at the first failure. */
  error: string | null;
};

/**
 * Materialize a planned manifest: for each slice in dependency order,
 * create its worktree off the resolved parent, reproduce its file set as
 * one commit, push, open a draft PR, record the PR into the manifest, and
 * track it in the engine. The wt list derives the parent relationship
 * from the manifest, so there's no separate display state to write. Tags
 * the holistic branch on success. Idempotent: slices already
 * `open`/`merged` are skipped.
 */
export async function applyStack(
  stackId: string,
  opts: ApplyOptions,
  onLog: Logger,
): Promise<ApplyResult> {
  const manifest = getStackManifest(stackId);
  if (!manifest) {
    return { materialized: [], skipped: [], error: `no stack manifest: ${stackId}` };
  }
  if (!manifest.holisticBranch) {
    return { materialized: [], skipped: [], error: `manifest ${stackId} has no holisticBranch` };
  }

  let ordered: StackSlice[];
  try {
    ordered = topoSortSlices(manifest);
  } catch (e) {
    return { materialized: [], skipped: [], error: e instanceof Error ? e.message : String(e) };
  }

  const materialized: string[] = [];
  const skipped: string[] = [];

  for (const slice of ordered) {
    if (slice.status !== "planned") {
      skipped.push(slice.id);
      onLog(`skip ${slice.id} (${slice.status})`);
      continue;
    }
    const parentBranch = resolveParentBranch(manifest, slice);
    const isRoot = isLaneRoot(slice);
    const parentRef = isRoot ? `origin/${config.branch.base}` : parentBranch;

    // Idempotent re-run: if a PR already exists on this branch (a prior
    // run materialized + pushed + opened the PR but failed before
    // recording it), adopt it instead of creating a duplicate.
    const existingPr = await viewPrInfo(slice.branch);
    if (existingPr) {
      onLog(`adopt ${slice.id} → existing PR #${existingPr.number}`);
      updateStackSlice(stackId, slice.id, {
        pr: existingPr.number,
        status: existingPr.state === "MERGED" ? "merged" : "open",
      });
      materialized.push(slice.id);
      if (!isRoot) await restackEngine.track(slice.branch, parentBranch);
      continue;
    }

    onLog(`apply ${slice.id} → ${slice.branch} (off ${parentRef})`);

    const created = await createWorktree(slice.branch, {
      base: parentRef,
      runInstall: opts.install === true,
      onLog: (l) => onLog(`  ${l}`),
    });
    if (!created.ok) {
      return { materialized, skipped, error: `create ${slice.branch}: ${created.reason}` };
    }

    const mat = await materializeSliceCommit(
      created.path,
      manifest.holisticBranch,
      slice,
      onLog,
    );
    if (!mat.ok) {
      return { materialized, skipped, error: `materialize ${slice.id}: ${mat.error}` };
    }

    const push = await gitRun(["push", "-u", "origin", slice.branch], created.path);
    if (push.exitCode !== 0) {
      return {
        materialized,
        skipped,
        error: `push ${slice.branch}: ${(push.stderr || push.stdout).trim()}`,
      };
    }
    onLog(`  pushed ${slice.branch}`);

    // gh wants a branch name for --base: the trunk name for a lane root,
    // else the parent slice's branch.
    const prBase = isRoot ? config.branch.base : parentBranch;
    const pr = await createDraftPr({
      cwd: created.path,
      head: slice.branch,
      base: prBase,
      title: slice.title,
      body: sliceBody(manifest, slice),
    });
    if (!pr.ok) {
      return { materialized, skipped, error: `pr create ${slice.branch}: ${pr.error}` };
    }
    onLog(`  opened draft PR #${pr.number}`);
    updateStackSlice(stackId, slice.id, { pr: pr.number, status: "open" });
    materialized.push(slice.id);

    // Seed the engine with the squash-safe restack metadata. Lane roots
    // aren't tracked — the engine rejects trunk parents. The wt list
    // derives the parent relationship straight from the manifest, so
    // there's no separate display state to seed.
    if (!isRoot) {
      const tracked = await restackEngine.track(slice.branch, parentBranch);
      if (!tracked.ok) {
        onLog(`  warn: stack track failed: ${tracked.stderr.trim() || tracked.exitCode}`);
      }
    }
  }

  // Archive the holistic branch as a tag so the origin node survives the
  // user rm'ing its worktree. `-f` so re-apply re-points cleanly.
  const tagName = `${stackId}-holistic`;
  const tag = await gitRun(["tag", "-f", tagName, manifest.holisticBranch]);
  if (tag.exitCode === 0) {
    patchStackManifest(stackId, { archivedTag: `refs/tags/${tagName}` });
    onLog(`tagged holistic branch → refs/tags/${tagName}`);
  } else {
    onLog(`warn: could not tag holistic branch: ${tag.stderr.trim()}`);
  }

  log.info("applied stack", { stackId, materialized, skipped });
  return { materialized, skipped, error: null };
}

// ---------- status ----------

export type SliceStatusRow = {
  slice: StackSlice;
  /** Branch the manifest intends this slice to stack on. */
  expectedBase: string;
  /** Live PR info from GitHub, or null when there's no PR / gh is absent. */
  live: LivePrInfo | null;
  /** Human description of any drift between manifest and reality; null when aligned. */
  drift: string | null;
};

export type StackStatusReport = {
  manifest: StackManifest;
  rows: SliceStatusRow[];
};

/**
 * Reconcile the manifest against live reality: for each slice, compare
 * the intended parent branch with the live PR base. Drift is reported,
 * never silently trusted in either direction.
 */
export async function stackStatus(stackId: string): Promise<StackStatusReport | null> {
  const manifest = getStackManifest(stackId);
  if (!manifest) return null;
  const rows = await Promise.all(
    manifest.slices
      .slice()
      .sort((a, b) => a.ordinal - b.ordinal)
      .map(async (slice): Promise<SliceStatusRow> => {
        const expectedBase = resolveParentBranch(manifest, slice);
        const live = slice.pr ? await viewPrInfo(slice.branch) : null;
        let drift: string | null = null;
        if (live && live.baseRefName && live.baseRefName !== expectedBase) {
          drift = `PR base is ${live.baseRefName}, manifest expects ${expectedBase}`;
        } else if (
          slice.status === "open" &&
          live &&
          live.state === "MERGED"
        ) {
          drift = `PR #${live.number} is merged but manifest says ${slice.status}`;
        } else if (slice.status === "open" && slice.pr && !live) {
          drift = `manifest records PR #${slice.pr} but GitHub has none`;
        }
        return { slice, expectedBase, live, drift };
      }),
  );
  return { manifest, rows };
}

// ---------- rebase / land ----------

export type RebaseOptions = {
  /** Trunk to reparent landed slices onto. Default `config.branch.base`. */
  onto?: string;
  /** Land via the merge queue (`stack merge --auto`) instead of `stack sync`. */
  merge?: boolean;
};

export type RebaseResult =
  | { ok: true; output: string }
  | {
      ok: false;
      conflict: boolean;
      error: string;
      failedBranch?: string;
      backupBranch?: string;
    };

/**
 * Maintenance wrapper. Regenerates the engine's links from the manifest
 * (so a sync never surprises us with PR-base inference), runs the engine
 * once per lane, then reconciles the manifest with post-run reality
 * (merged slices flip to `merged`; their children reparent onto trunk).
 *
 * On an engine conflict bail, returns the failing + backup branch so the
 * calling skill resolves it — wt never auto-resolves conflicts.
 * Concurrency is enforced inside the engine via a cross-process flock.
 */
export async function rebaseStack(
  stackId: string,
  opts: RebaseOptions,
  onLog: Logger,
): Promise<RebaseResult> {
  const manifest = getStackManifest(stackId);
  if (!manifest) {
    return { ok: false, conflict: false, error: `no stack manifest: ${stackId}` };
  }
  const trunk = opts.onto ?? config.branch.base;

  // 1. Regenerate engine links from the manifest (children only).
  for (const slice of manifest.slices) {
    if (isLaneRoot(slice)) continue;
    const parent = resolveParentBranch(manifest, slice);
    const tracked = await restackEngine.track(slice.branch, parent);
    if (!tracked.ok) {
      onLog(`warn: track ${slice.branch} onto ${parent}: ${tracked.stderr.trim()}`);
    }
  }

  // 2. Drive the engine once per lane root so independent lanes are each
  //    scoped (`stack sync/merge <branch>` operates on that branch's stack).
  const laneRoots = manifest.slices.filter(isLaneRoot);
  const outputs: string[] = [];
  for (const root of laneRoots) {
    onLog(`${opts.merge ? "merge" : "sync"} lane ${root.branch}`);
    const res: EngineResult = opts.merge
      ? await restackEngine.merge(root.branch, { auto: true })
      : await restackEngine.sync(root.branch, { apply: true });
    outputs.push(res.stdout.trim());
    if (!res.ok) {
      if (res.conflict) {
        return {
          ok: false,
          conflict: true,
          error: `engine bailed on conflict in lane ${root.branch}`,
          ...(res.failedBranch ? { failedBranch: res.failedBranch } : {}),
          ...(res.backupBranch ? { backupBranch: res.backupBranch } : {}),
        };
      }
      return {
        ok: false,
        conflict: false,
        error: (res.stderr || res.stdout).trim() || `engine exited ${res.exitCode}`,
      };
    }
  }

  // 3. Reconcile the manifest with reality: a merged slice flips to
  //    `merged`; its children reparent onto trunk (drop the dep, base→trunk).
  await reconcileMerged(stackId, trunk, onLog);

  return { ok: true, output: outputs.join("\n") };
}

/** Flip merged slices + reparent their orphaned children. */
async function reconcileMerged(
  stackId: string,
  trunk: string,
  onLog: Logger,
): Promise<void> {
  const manifest = getStackManifest(stackId);
  if (!manifest) return;
  // Probe live PR state for every candidate slice in parallel.
  const candidates = manifest.slices.filter(
    (s) => s.pr && s.status !== "merged",
  );
  const probed = await Promise.all(
    candidates.map(async (s) => ({ s, live: await viewPrInfo(s.branch) })),
  );
  const mergedIds = new Set<string>(
    manifest.slices.filter((s) => s.status === "merged").map((s) => s.id),
  );
  for (const { s, live } of probed) {
    if (live?.state === "MERGED") {
      mergedIds.add(s.id);
      updateStackSlice(stackId, s.id, { status: "merged" });
      onLog(`slice ${s.id} merged (#${s.pr})`);
    }
  }
  if (mergedIds.size === 0) return;

  // Reparent each surviving slice that lost a dependency onto its
  // deepest STILL-OPEN dependency (highest ordinal), falling to trunk
  // only when none survive. Reparenting straight to trunk would flatten
  // a slice that still has a live ancestor (diamond / multi-parent).
  const fresh = getStackManifest(stackId);
  if (!fresh) return;
  const byId = new Map(fresh.slices.map((s) => [s.id, s]));
  for (const slice of fresh.slices) {
    if (slice.status === "merged") continue;
    const dependsOn = slice.dependsOn.filter((d) => !mergedIds.has(d));
    const baseMerged = mergedIds.has(slice.base);
    if (dependsOn.length === slice.dependsOn.length && !baseMerged) continue;
    const survivingParent = dependsOn
      .map((d) => byId.get(d))
      .filter((s): s is StackSlice => !!s)
      .sort((a, b) => b.ordinal - a.ordinal)[0];
    const base = survivingParent ? survivingParent.id : trunk;
    // The list reads the parent straight from the manifest, so updating
    // `base`/`dependsOn` is all that's needed — no separate display state.
    updateStackSlice(stackId, slice.id, { dependsOn, base });
    onLog(`reparented ${slice.id} onto ${base}`);
  }
}
