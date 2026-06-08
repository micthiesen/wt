/**
 * Materialization + maintenance for stack manifests. wt owns the
 * manifest (truth); this module turns a planned manifest into real
 * worktrees, commits, and draft PRs (`applyStack`), reports the manifest
 * DAG against live reality (`stackStatus`), reconciles the manifest with
 * landed PRs (`reconcileStack`), and drives the native squash-safe engine
 * to replay slices onto their (possibly rewritten) parents (`replayStack`).
 * `rebaseStack` is the thin reconcile-then-replay convenience. The genuinely
 * hard part (anchored cherry-pick replay) lives in `RestackEngine`.
 */
import { config } from "./config.ts";
import { gitQuiet, gitRun } from "./git.ts";
import { createWorktree } from "./lifecycle.ts";
import { tryAcquireLock } from "./locks.ts";
import { createLogger } from "./logger.ts";
import {
  createDraftPr,
  retargetPrBase,
  viewPrInfo,
  type LivePrInfo,
} from "./github.ts";
import { restackEngine } from "./restack-engine.ts";
import {
  isTrunkBase,
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
import { listWorktrees, worktreeIsDirty } from "./worktree.ts";

const log = createLogger("[stack-ops]");

/** Flock slug serializing replay across processes (one restack at a time). */
const STACK_LOCK_SLUG = "__stack__";

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
    // A trunk-based slice branches off `origin/<trunk>` and targets its PR
    // at trunk; any other slice (a stacked child, or a root stacked on an
    // unmerged parent PR) branches off + targets its resolved parent
    // branch. The engine only tracks non-trunk parents.
    const onTrunk = isTrunkBase(slice);
    let parentRef: string;
    if (onTrunk) {
      parentRef = `origin/${config.branch.base}`;
    } else {
      // Prefer the local parent branch (a sibling slice just materialized
      // it, or it's the user's parent-PR worktree); fall back to the
      // remote-tracking ref when the parent exists only on origin, so
      // `git worktree add` resolves a real ref either way.
      const localParent = await gitQuiet(
        ["show-ref", "--verify", "--quiet", `refs/heads/${parentBranch}`],
      );
      parentRef = localParent ? parentBranch : `origin/${parentBranch}`;
    }

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
      continue;
    }

    onLog(`apply ${slice.id} → ${slice.branch} (off ${parentRef})`);

    // Slices are install-free by design (a slice == a light worktree, no
    // node_modules), so do NOT add a per-slice typecheck/build gate here —
    // it can't run. Verification is the skill's job, done BEFORE apply in a
    // dep-having checkout; per-slice CI is the backstop. `--install` is an
    // explicit opt-in, default off.
    const created = await createWorktree(slice.branch, {
      base: parentRef,
      runInstall: opts.install === true,
      onLog: (l) => onLog(`  ${l}`),
    });
    if (!created.ok) {
      return { materialized, skipped, error: `create ${slice.branch}: ${created.reason}` };
    }

    // Record the squash-safe replay anchor: the parent tip this slice's
    // commit will sit on. `createWorktree` started the branch at `parentRef`,
    // so HEAD is that tip right now, before the slice commit lands on top.
    const baseSha = await revParseAt(created.path, "HEAD");

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

    // gh wants a branch name for --base: the trunk name for a trunk-based
    // slice, else the resolved parent branch (sibling slice or external
    // parent PR branch).
    const prBase = onTrunk ? config.branch.base : parentBranch;
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
    updateStackSlice(stackId, slice.id, {
      pr: pr.number,
      status: "open",
      ...(baseSha ? { baseSha } : {}),
    });
    materialized.push(slice.id);
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

// ---------- reconcile / replay / rebase ----------

export type RebaseOptions = {
  /** Trunk that landed roots reparent onto. Default `config.branch.base`. */
  onto?: string;
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
 * The one-shot /restack convenience: reconcile the manifest against landed
 * PRs, then replay every surviving slice onto its (possibly rewritten)
 * parent. `reconcileStack` and `replayStack` are exposed separately so the
 * skill can drive them step-by-step around a conflict (reconcile once,
 * replay → resolve → replay again).
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
  await reconcileStack(stackId, trunk, onLog);
  return replayStack(stackId, { onto: trunk }, onLog);
}

/**
 * Replay every surviving slice onto its parent, squash-safe, in topological
 * order: rebase the slice's own commits in its own worktree, force-push, and
 * retarget its PR base to match the manifest. Bails clean on the first
 * conflict, naming the slice + the backup branch the engine left — wt never
 * auto-resolves. Pure git + gh; does NOT reconcile (run `reconcileStack`
 * first for a post-merge restack). Serialized across processes by a flock.
 */
export async function replayStack(
  stackId: string,
  opts: RebaseOptions,
  onLog: Logger,
): Promise<RebaseResult> {
  const handle = tryAcquireLock(STACK_LOCK_SLUG, "stack", { phase: "replay" });
  if (!handle) {
    return {
      ok: false,
      conflict: false,
      error: "another wt stack operation is already running",
    };
  }
  try {
    return await replayStackLocked(stackId, opts, onLog);
  } finally {
    handle.release();
  }
}

async function replayStackLocked(
  stackId: string,
  opts: RebaseOptions,
  onLog: Logger,
): Promise<RebaseResult> {
  const manifest = getStackManifest(stackId);
  if (!manifest) {
    return { ok: false, conflict: false, error: `no stack manifest: ${stackId}` };
  }
  const trunk = opts.onto ?? config.branch.base;

  // Topo order so each parent is replayed before its children; merged slices
  // drop out (their branch is gone, their children already reparented).
  let ordered: StackSlice[];
  try {
    ordered = topoSortSlices(manifest);
  } catch (e) {
    return { ok: false, conflict: false, error: e instanceof Error ? e.message : String(e) };
  }
  const live = ordered.filter((s) => s.status !== "merged");
  const byId = new Map(manifest.slices.map((s) => [s.id, s]));

  // Each slice replays IN ITS OWN WORKTREE (HEAD rebases in place), so map
  // branch → path and refuse any dirty one up front — a rebase would clobber.
  const pathByBranch = new Map(
    (await listWorktrees())
      .filter((w) => !w.isMain && w.branch)
      .map((w) => [w.branch, w.path] as const),
  );
  for (const s of live) {
    const p = pathByBranch.get(s.branch);
    if (p && (await worktreeIsDirty(p))) {
      return {
        ok: false,
        conflict: false,
        error: `slice worktree ${p} (${s.branch}) has uncommitted changes — commit or stash before restacking`,
      };
    }
  }

  // Freshen origin so `origin/<trunk>` and any external-parent ref resolve to
  // their live tips before we rebase onto them. A failed fetch would silently
  // leave stale refs and replay every slice onto an outdated base, so bail.
  const fetched = await gitRun(["fetch", "origin", "--quiet"]);
  if (fetched.exitCode !== 0) {
    return {
      ok: false,
      conflict: false,
      error: `git fetch origin failed (${(fetched.stderr || fetched.stdout).trim() || `exit ${fetched.exitCode}`}) — refusing to replay onto possibly-stale refs`,
    };
  }

  // Pass 1: resolve each slice's worktree + anchor (the old parent tip its
  // commits sit on) BEFORE any rewrite — so the merge-base fallback sees
  // pre-replay tips, AND so a missing worktree or unresolvable anchor fails
  // the whole run before a single slice has been pushed.
  const anchorById = new Map<string, string>();
  for (const s of live) {
    const p = pathByBranch.get(s.branch);
    if (!p) {
      return {
        ok: false,
        conflict: false,
        error: `slice ${s.id} (${s.branch}) has no worktree — recreate it with \`wt stack apply ${stackId}\``,
      };
    }
    const anchor = await resolveAnchor(s, byId, trunk, p);
    if (!anchor) {
      return {
        ok: false,
        conflict: false,
        error: `could not resolve a replay anchor for ${s.branch} (no baseSha and no merge-base)`,
      };
    }
    anchorById.set(s.id, anchor);
  }

  // Pass 2: replay top-down, threading each slice's new tip to its children.
  // Worktree + anchor are guaranteed present from pass 1.
  const newTipById = new Map<string, string>();
  let replayed = 0;
  for (const s of live) {
    const worktreePath = pathByBranch.get(s.branch)!;
    const anchor = anchorById.get(s.id)!;
    const newBase = await resolveNewBaseSha(s, byId, trunk, newTipById, worktreePath);
    if (!newBase) {
      return {
        ok: false,
        conflict: false,
        error: `could not resolve the new base for ${s.branch}`,
      };
    }

    onLog(`replay ${s.id} (${s.branch})`);
    const out = await restackEngine.replaySlice(
      { branch: s.branch, worktreePath, anchor, newBase },
      onLog,
    );
    if (!out.ok) {
      if (out.conflict) {
        return {
          ok: false,
          conflict: true,
          error: `conflict replaying ${s.branch} — resolve in its worktree, then re-run`,
          failedBranch: s.branch,
          backupBranch: out.backupBranch,
        };
      }
      return { ok: false, conflict: false, error: out.error };
    }
    newTipById.set(s.id, out.newTip);
    // Advance the stored anchor to the parent we just landed on, so the next
    // restack is a cheap no-op when nothing has moved.
    updateStackSlice(stackId, s.id, { baseSha: out.newBaseSha });

    // Keep the PR base aligned with the manifest only when the slice actually
    // moved (a parent that landed/rewrote shifts the child onto a new base).
    // An unmoved slice's base is already correct, so we skip the `gh pr view`
    // probe — any residual drift still surfaces in `wt stack status`.
    if (out.moved) {
      replayed++;
      await retargetIfNeeded(s, resolveParentBranch(manifest, s), onLog);
    }
  }

  return { ok: true, output: `replayed ${replayed}/${live.length} slice(s)` };
}

/** rev-parse a ref to its commit SHA in `cwd`, or null if it doesn't resolve. */
async function revParseAt(cwd: string, ref: string): Promise<string | null> {
  const r = await gitRun(["rev-parse", "--verify", "--quiet", `${ref}^{commit}`], cwd);
  const sha = r.stdout.trim();
  return r.exitCode === 0 && sha ? sha : null;
}

/** First resolvable ref among `refs`, as a SHA — local branch then origin. */
async function firstSha(cwd: string, refs: string[]): Promise<string | null> {
  for (const ref of refs) {
    const sha = await revParseAt(cwd, ref);
    if (sha) return sha;
  }
  return null;
}

/**
 * The parent-tip SHA a slice currently sits on (the `rebase --onto` anchor):
 * the stored `baseSha` when present, else the merge-base of the slice and its
 * current parent ref — computed before any replay so siblings haven't moved.
 */
async function resolveAnchor(
  slice: StackSlice,
  byId: Map<string, StackSlice>,
  trunk: string,
  cwd: string,
): Promise<string | null> {
  if (slice.baseSha) return slice.baseSha;
  const parentRef = currentParentRef(slice, byId, trunk);
  const r = await gitRun(["merge-base", slice.branch, parentRef], cwd);
  const sha = r.stdout.trim();
  return r.exitCode === 0 && sha ? sha : null;
}

/** The ref naming a slice's parent tip as it stands now (pre-replay). */
function currentParentRef(
  slice: StackSlice,
  byId: Map<string, StackSlice>,
  trunk: string,
): string {
  if (isTrunkBase(slice)) return `origin/${trunk}`;
  const sibling = byId.get(slice.base);
  if (sibling) return sibling.branch;
  return slice.base; // external parent branch
}

/**
 * The SHA to rebase a slice ONTO this run: trunk's freshly-fetched tip, the
 * parent sibling's just-replayed tip, or an external parent branch's live
 * tip (a stack stacked on an unmerged parent PR / another stack's tip).
 */
async function resolveNewBaseSha(
  slice: StackSlice,
  byId: Map<string, StackSlice>,
  trunk: string,
  newTipById: Map<string, string>,
  cwd: string,
): Promise<string | null> {
  if (isTrunkBase(slice)) return revParseAt(cwd, `origin/${trunk}`);
  const sibling = byId.get(slice.base);
  if (sibling) {
    const replayed = newTipById.get(sibling.id);
    return replayed ?? revParseAt(cwd, sibling.branch);
  }
  // External parent branch: prefer the local checkout, fall back to origin.
  return firstSha(cwd, [slice.base, `origin/${slice.base}`]);
}

/** Retarget a slice's PR base to `expectedBase` when GitHub disagrees. */
async function retargetIfNeeded(
  slice: StackSlice,
  expectedBase: string,
  onLog: Logger,
): Promise<void> {
  if (!slice.pr) return;
  const live = await viewPrInfo(slice.branch);
  if (!live || live.baseRefName === expectedBase) return;
  const r = await retargetPrBase(slice.pr, expectedBase);
  if (r.ok) onLog(`  retargeted PR #${slice.pr} base → ${expectedBase}`);
  else onLog(`  warn: retarget PR #${slice.pr} base: ${r.error}`);
}

/**
 * Reconcile the manifest against landed reality: flip merged slices to
 * `merged` and reparent each orphaned child onto its deepest surviving
 * dependency (or trunk). Pure manifest bookkeeping — no git, no replay — so
 * the skill can run it on its own before deciding to replay.
 */
export async function reconcileStack(
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
