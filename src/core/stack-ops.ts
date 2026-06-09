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
import { branchExists, gitQuiet, gitRun } from "./git.ts";
import { createWorktree } from "./lifecycle.ts";
import { tryAcquireLock } from "./locks.ts";
import { createLogger } from "./logger.ts";
import {
  createDraftPr,
  retargetPrBase,
  viewPrInfo,
  type LivePrInfo,
} from "./github.ts";
import { backupBranchOwner, backupTimestamp, rebaseInProgress, restackEngine } from "./restack-engine.ts";
import {
  isTrunkBase,
  resolveParentBranch,
  topoSortSlices,
} from "./stack-layout.ts";
import {
  findStackIdByBranch,
  getStackManifest,
  patchStackManifest,
  putStackManifest,
  updateStackSlice,
  validateStackManifest,
  type StackManifest,
  type StackSlice,
} from "./wtstate.ts";
import { listWorktrees, worktreeHasTrackedChanges } from "./worktree.ts";

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

    // A re-split sub-slice reproduces its files from the original slice's
    // branch (`slice.source`) — which carries content the pre-split holistic
    // branch predates — rather than the manifest holistic.
    const mat = await materializeSliceCommit(
      created.path,
      slice.source ?? manifest.holisticBranch,
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
  // user rm'ing its worktree. `-f` so re-apply re-points cleanly. On a
  // re-apply (e.g. `wt stack split` → `apply`) the holistic branch has usually
  // already been archived to its tag and deleted — the tag still anchors the
  // origin node, so skip silently instead of warning on an unresolvable ref.
  const tagName = `${stackId}-holistic`;
  const holisticResolves = await gitQuiet([
    "rev-parse",
    "--verify",
    "--quiet",
    `${manifest.holisticBranch}^{commit}`,
  ]);
  if (holisticResolves) {
    const tag = await gitRun(["tag", "-f", tagName, manifest.holisticBranch]);
    if (tag.exitCode === 0) {
      patchStackManifest(stackId, { archivedTag: `refs/tags/${tagName}` });
      onLog(`tagged holistic branch → refs/tags/${tagName}`);
    } else {
      onLog(`warn: could not tag holistic branch: ${tag.stderr.trim()}`);
    }
  } else if (!manifest.archivedTag) {
    onLog(`warn: holistic branch ${manifest.holisticBranch} not found and no archived tag to anchor the origin node`);
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
  // Only OPEN slices replay. Merged ones dropped out at reconcile, and a
  // `planned` slice isn't materialized — no PR, and any branch/worktree
  // already sitting under it is hand-authored WIP the engine must neither
  // rebase nor gate on (a dirty planned tip used to block the whole stack).
  // Skip it loudly; it catches up at `wt stack apply` / `wt stack add`.
  const live = ordered.filter((s) => s.status === "open");
  for (const s of ordered) {
    if (s.status === "planned") {
      onLog(`skip ${s.id} (${s.branch}) — planned slice, not yet materialized`);
    }
  }
  const byId = new Map(manifest.slices.map((s) => [s.id, s]));

  // Each slice replays IN ITS OWN WORKTREE (HEAD rebases in place), so map
  // branch → path and refuse any dirty one up front — a rebase would clobber.
  // Only TRACKED changes block: untracked files ride through a rebase safely
  // (git refuses cleanly if one would be overwritten), and the workflow itself
  // drops untracked files like `prompt.txt` into slice worktrees.
  const pathByBranch = new Map(
    (await listWorktrees())
      .filter((w) => !w.isMain && w.branch)
      .map((w) => [w.branch, w.path] as const),
  );
  for (const s of live) {
    const p = pathByBranch.get(s.branch);
    if (!p) continue;
    if (await worktreeHasTrackedChanges(p)) {
      return {
        ok: false,
        conflict: false,
        error: `slice ${s.id} worktree ${p} (${s.branch}) has uncommitted changes to tracked files — commit or stash before restacking`,
      };
    }
    // A worktree left mid-rebase by an earlier interrupted run can read clean
    // via `git status --porcelain` (no unmerged paths), so the dirty check
    // alone misses it. Replaying into it would make the engine abort and
    // silently discard that in-flight state — refuse up front instead.
    if (await rebaseInProgress(p)) {
      return {
        ok: false,
        conflict: false,
        error: `slice worktree ${p} (${s.branch}) is mid-rebase from an unfinished run — finish or \`git rebase --abort\` it there before restacking`,
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
        error: `could not resolve a replay anchor for ${s.branch} (no baseSha and no merge-base)${plannedParentHint(s, byId)}`,
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
        error: `could not resolve the new base for ${s.branch}${plannedParentHint(s, byId)}`,
      };
    }

    onLog(`replay ${s.id} (${s.branch})`);
    const out = await restackEngine.replaySlice(
      { branch: s.branch, worktreePath, anchor, newBase },
      onLog,
    );
    if (!out.ok) {
      // Persist the failure to the daily app log — the engine only streams to
      // the console `onLog`, so a replay run from the CLI would otherwise leave
      // nothing to diagnose after the fact.
      log.warn("replay slice failed", {
        stackId,
        slice: s.id,
        branch: s.branch,
        conflict: out.conflict,
        worktree: worktreePath,
        anchor,
        newBase,
        error: out.error,
        // backupBranch only exists on the conflict variant — it's the recovery
        // handle, so log it when present.
        ...(out.conflict ? { backupBranch: out.backupBranch } : {}),
      });
      if (out.conflict) {
        return {
          ok: false,
          conflict: true,
          // `out.error` carries the engine's conflicting-file detail; keep it
          // so the operator sees WHICH files clashed at the CLI, not just in
          // the log.
          error: `${out.error} — resolve in its worktree, then re-run`,
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

    // Keep the PR base aligned with the manifest when the slice actually
    // moved (a parent that landed/rewrote shifts the child onto a new base)
    // OR when the engine synced a stale remote — a hand-resolved conflict may
    // have changed the parent too. A slice that neither moved nor pushed has
    // a correct base already, so we skip the `gh pr view` probe — any
    // residual drift still surfaces in `wt stack status`.
    if (out.moved) replayed++;
    if (out.moved || out.pushed) {
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
 * the stored `baseSha` when it's still honest, else the merge-base of the slice
 * and its current parent ref — computed before any replay so siblings haven't moved.
 *
 * The stored `baseSha` is the squash-safe cut point (the parent tip this
 * slice's commits were last based on) and is trusted ONLY while it's still an
 * ancestor of the branch. A conflict bail hands resolution to a human, who
 * rebases the slice by hand and force-pushes WITHOUT updating the manifest — so
 * the stored anchor now points at a tip the branch no longer descends from.
 * Replaying off that stale anchor re-applies the parent's already-present
 * commits onto themselves, a bogus conflict on an already-correct slice (this
 * bit the eng-5182 restack + re-split twice). When the anchor is stale, fall
 * back to the live merge-base with the current parent, which post-rebase is
 * exactly the parent tip the slice now sits on — self-healing, no manual
 * bookkeeping. (The healthy squash case keeps `baseSha`: it's still an ancestor
 * of the unrewritten child, so a squash-merged parent's commits stay excluded.)
 */
async function resolveAnchor(
  slice: StackSlice,
  byId: Map<string, StackSlice>,
  trunk: string,
  cwd: string,
): Promise<string | null> {
  if (slice.baseSha) {
    const stillAncestor = await gitQuiet(
      ["merge-base", "--is-ancestor", slice.baseSha, slice.branch],
      cwd,
    );
    if (stillAncestor) return slice.baseSha;
  }
  const parentRef = currentParentRef(slice, byId, trunk);
  const r = await gitRun(["merge-base", slice.branch, parentRef], cwd);
  const sha = r.stdout.trim();
  return r.exitCode === 0 && sha ? sha : null;
}

/**
 * Failure hint for a slice whose parent is still `planned`: replay skips
 * planned slices, so the parent's branch may not exist yet — the likely
 * reason an anchor or new base failed to resolve.
 */
function plannedParentHint(slice: StackSlice, byId: Map<string, StackSlice>): string {
  const parent = byId.get(slice.base);
  return parent?.status === "planned"
    ? ` — parent ${parent.id} is still planned; materialize it with \`wt stack apply\` first`
    : "";
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
 * `merged`, reparent each orphaned child onto its deepest surviving
 * dependency (or trunk), and reparent a slice whose EXTERNAL parent
 * (stack-on-stack) has landed onto trunk. Manifest bookkeeping only — reads
 * GitHub/git state but never rewrites branches — so the skill can run it on
 * its own before deciding to replay.
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
  if (mergedIds.size > 0) {
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

  // Cross-stack reconcile: a slice stacked on an EXTERNAL parent (another
  // stack's tip, or a standalone parent PR branch) keeps a dead `base` once
  // that parent lands — the own-slice probe above only sees THIS manifest's
  // PRs, so it can't notice. Detect the external parent merged (or its
  // branch gone) and reparent onto trunk. The slice's `baseSha` anchor keeps
  // the subsequent replay squash-safe: the landed parent's commits sit below
  // the anchor and are excluded by construction, exactly like a sibling
  // squash-merge. Runs unconditionally — the external parent merging is
  // invisible to `mergedIds`.
  const after = getStackManifest(stackId);
  if (!after) return;
  const siblingIds = new Set(after.slices.map((s) => s.id));
  const siblingBranches = new Set(after.slices.map((s) => s.branch));
  for (const slice of after.slices) {
    if (slice.status === "merged") continue;
    if (slice.base === trunk || isTrunkBase(slice)) continue;
    if (siblingIds.has(slice.base) || siblingBranches.has(slice.base)) continue;
    const live = await viewPrInfo(slice.base);
    if (live?.state === "MERGED") {
      updateStackSlice(stackId, slice.id, { base: trunk });
      onLog(`external parent ${slice.base} merged (#${live.number}) — reparented ${slice.id} onto ${trunk}`);
    } else if (!live && !(await branchExists(slice.base))) {
      // No PR and no branch anywhere — the parent is gone. (A CLOSED PR or a
      // still-open parent leaves the link alone.)
      updateStackSlice(stackId, slice.id, { base: trunk });
      onLog(`external parent ${slice.base} is gone — reparented ${slice.id} onto ${trunk}`);
    }
  }
}

// ---------- split (reshape a live stack) ----------

/** One sub-slice in a `splitStack` fragment — the planner (`/split`) authors these. */
export type SubSliceSpec = {
  id: string;
  title: string;
  branch: string;
  files: string[];
  oversized?: boolean;
  oversizedReason?: string;
};

export type SplitResult =
  | {
      ok: true;
      newSliceIds: string[];
      /** Branch the new sub-slices reproduce their files from at materialize. */
      sourceBranch: string;
      /** Branch + PR of the replaced slice — supersede (close PR, delete branch) after apply. */
      supersededBranch: string;
      supersededPr: number | null;
      /** Branches of children re-threaded onto the new tip (need a `replay`). */
      rethreadedChildren: string[];
      /** The reshaped slice list (returned for `--plan` preview; written unless planning). */
      slices: StackSlice[];
    }
  | { ok: false; error: string };

/**
 * Reshape a live stack: replace one OPEN (or still-planned) slice with the N
 * sub-slices in `fragment`, chaining them in order and re-threading the
 * replaced slice's children onto the LAST sub-slice (the new tip). Pure
 * manifest bookkeeping — no git, no PRs — mirroring `reconcileStack`. The
 * caller then runs `wt stack apply` to materialize the new sub-slice branches
 * (sourced from the replaced slice's own branch, recorded as their `source`,
 * since it carries a refactor the pre-split holistic branch predates) and
 * `wt stack replay`/`R` to rebase the descendants onto the new tip.
 *
 * The replaced slice is REMOVED from the manifest, but its branch/PR are left
 * on GitHub: the branch is the materialize source, so it must survive until
 * `apply` — close the PR + delete the branch as superseded afterwards.
 *
 * Strictly validates the reshaped manifest via `validateStackManifest`
 * (catches duplicate ids/branches, dangling deps, empty file sets) before
 * writing. `opts.plan` validates + returns the new shape WITHOUT writing.
 */
export function splitStack(
  stackId: string,
  sliceId: string,
  fragment: SubSliceSpec[],
  opts: { plan?: boolean },
): SplitResult {
  const manifest = getStackManifest(stackId);
  if (!manifest) return { ok: false, error: `no stack manifest: ${stackId}` };
  const target = manifest.slices.find((s) => s.id === sliceId);
  if (!target) return { ok: false, error: `no slice "${sliceId}" in ${stackId}` };
  if (target.status === "merged") {
    return { ok: false, error: `slice ${sliceId} is merged — cannot re-split a landed slice` };
  }
  if (fragment.length < 2) {
    return { ok: false, error: `split needs ≥2 sub-slices (got ${fragment.length})` };
  }
  // Reusing the replaced slice's own id/branch would make a sub-slice source
  // itself — reject it so the partition stays unambiguous.
  for (const spec of fragment) {
    if (spec.id === target.id) return { ok: false, error: `sub-slice id "${spec.id}" reuses the slice being split` };
    if (spec.branch === target.branch) return { ok: false, error: `sub-slice branch "${spec.branch}" reuses the slice being split` };
  }

  // The sub-slices reproduce their files from the original slice's branch (it
  // carries the refactor); a still-planned target has no branch yet, so fall
  // back to whatever the target itself would have materialized from.
  const source = target.status === "planned" ? target.source : target.branch;

  // Build the sub-chain: the first sub-slice takes the target's place in the
  // graph (inherits its base + dependsOn); each later one stacks on the
  // previous. All start planned, content sourced from the original branch.
  const lastSubId = fragment[fragment.length - 1]!.id;
  const subSlices: StackSlice[] = fragment.map((spec, i) => ({
    id: spec.id,
    ordinal: 0, // renumbered below
    title: spec.title,
    branch: spec.branch,
    base: i === 0 ? target.base : fragment[i - 1]!.id,
    dependsOn: i === 0 ? [...target.dependsOn] : [fragment[i - 1]!.id],
    files: spec.files,
    pr: null,
    status: "planned" as const,
    oversized: spec.oversized === true,
    ...(spec.oversizedReason ? { oversizedReason: spec.oversizedReason } : {}),
    ...(source ? { source } : {}),
  }));

  // Re-thread the target's children onto the new tip (the last sub-slice).
  // A child may reference the target by slice id OR by branch name — `base`
  // is stored either way (`dependsOn` is always ids) — so match both and
  // normalize the rewrite to the sub-slice id (`resolveParentBranch` resolves
  // it to the branch, and the id is stable before the branch is materialized).
  const rethreaded: string[] = [];
  const refsTarget = (ref: string): boolean => ref === target.id || ref === target.branch;
  const rethread = (s: StackSlice): StackSlice => {
    const changed = refsTarget(s.base) || s.dependsOn.some(refsTarget);
    if (changed) rethreaded.push(s.branch);
    return {
      ...s,
      base: refsTarget(s.base) ? lastSubId : s.base,
      dependsOn: s.dependsOn.map((d) => (refsTarget(d) ? lastSubId : d)),
    };
  };

  // Splice the sub-chain into the target's slot (preserving display order),
  // re-threading every other slice, then renumber ordinals over the new order.
  // The branch `-NN-` token is historical and may now diverge from `ordinal`.
  const inOrder = [...manifest.slices].sort((a, b) => a.ordinal - b.ordinal);
  const reshaped: StackSlice[] = [];
  for (const s of inOrder) {
    if (s.id === target.id) reshaped.push(...subSlices);
    else reshaped.push(rethread(s));
  }
  reshaped.forEach((s, i) => {
    s.ordinal = i + 1;
  });

  const next: StackManifest = { ...manifest, slices: reshaped };
  const v = validateStackManifest(next);
  if (!v.ok) {
    return { ok: false, error: `reshaped manifest invalid:\n  ${v.errors.join("\n  ")}` };
  }
  if (!opts.plan) putStackManifest(v.manifest);
  return {
    ok: true,
    newSliceIds: subSlices.map((s) => s.id),
    sourceBranch: source ?? manifest.holisticBranch,
    supersededBranch: target.branch,
    supersededPr: target.pr,
    rethreadedChildren: rethreaded,
    slices: v.manifest.slices,
  };
}

// ---------- add (append an existing branch to a live stack) ----------

export type AddSliceResult =
  | {
      ok: true;
      slice: StackSlice;
      /** Branch the new slice stacks on (a sibling's branch, or trunk). */
      parentBranch: string;
      /** Whether the slice's PR pre-existed or `add` opened it. */
      prAction: "adopted" | "created";
    }
  | { ok: false; error: string };

/** Fallback slice title from a branch name: strip the namespace + issue-id
 *  (+ optional ordinal) prefix, de-kebab the rest. `--title` overrides. */
function titleFromBranch(branch: string): string {
  const tail = branch.split("/").pop() ?? branch;
  const stripped = tail.replace(/^[a-z]+-\d+-(\d+[a-z]?-)?/i, "").replace(/-/g, " ").trim();
  return stripped || tail;
}

/** The ref (`branch` or `origin/branch`) that resolves in the main clone. */
async function localOrOriginRef(branch: string): Promise<string> {
  const local = await gitQuiet(["show-ref", "--verify", "--quiet", `refs/heads/${branch}`]);
  return local ? branch : `origin/${branch}`;
}

/**
 * Append an EXISTING branch to a live stack as a new tip slice — the inverse
 * of `splitStack`'s reshape, and the registration path for "I `wt new --base
 * <tip>`'d a branch on top of the stack, now track it". Purely additive
 * (existing slices untouched), which is what lets it work on a materialized
 * stack — the re-ingest path refuses those wholesale.
 *
 * Never creates a branch or worktree (`wt new` owns that); errors when the
 * branch doesn't exist. DOES ensure the slice has a PR: adopts an open one,
 * else pushes + opens a draft PR against the parent. That isn't scope creep —
 * `validateStackManifest` hard-rejects `open` without a `pr`, and a `planned`
 * slice would later be re-materialized by `applyStack` from the HOLISTIC
 * branch, clobbering an externally-authored branch's content. PR-or-create is
 * what keeps `apply` permanently away from this slice.
 *
 * The squash-safe anchor (`baseSha`) is recorded as `merge-base(branch,
 * parent)` — not the parent's tip, which may have advanced since the fork and
 * wouldn't be an ancestor of the branch.
 */
export async function addSliceToStack(
  stackId: string,
  branch: string,
  opts: { onto?: string; title?: string },
  onLog: Logger,
): Promise<AddSliceResult> {
  const manifest = getStackManifest(stackId);
  if (!manifest) return { ok: false, error: `no stack manifest: ${stackId}` };
  const owner = findStackIdByBranch(branch);
  if (owner) {
    return { ok: false, error: `${branch} is already tracked by stack ${owner}` };
  }
  if (!(await branchExists(branch))) {
    return {
      ok: false,
      error: `branch ${branch} not found (local or origin) — \`add\` registers an existing branch; create it first (e.g. \`wt new … --base <parentTip>\`)`,
    };
  }

  // Resolve the parent: `--onto` names a sibling slice (by id or branch) or
  // trunk (a new parallel lane root); default is the stack tip — the
  // highest-ordinal live slice.
  let parentSlice: StackSlice | null = null;
  if (opts.onto && opts.onto !== config.branch.base) {
    parentSlice =
      manifest.slices.find((s) => s.id === opts.onto || s.branch === opts.onto) ?? null;
    if (!parentSlice) {
      return {
        ok: false,
        error: `--onto ${opts.onto} matches no slice in ${stackId} (pass a slice id, a slice branch, or ${config.branch.base})`,
      };
    }
    if (parentSlice.status === "merged") {
      return {
        ok: false,
        error: `slice ${parentSlice.id} is merged — stack on a live slice, or --onto ${config.branch.base}`,
      };
    }
  } else if (!opts.onto) {
    parentSlice =
      manifest.slices
        .filter((s) => s.status !== "merged")
        .sort((a, b) => b.ordinal - a.ordinal)[0] ?? null;
    if (!parentSlice) {
      return {
        ok: false,
        error: `stack ${stackId} has no live slices — pass --onto ${config.branch.base} to root a new lane`,
      };
    }
  }
  const parentBranch = parentSlice ? parentSlice.branch : config.branch.base;

  const branchRef = await localOrOriginRef(branch);
  const parentRef = parentSlice
    ? await localOrOriginRef(parentBranch)
    : `origin/${config.branch.base}`;

  // Anchor = the cut point the branch's own commits sit on.
  const mb = await gitRun(["merge-base", branchRef, parentRef]);
  const baseSha = mb.stdout.trim();
  if (mb.exitCode !== 0 || !baseSha) {
    return {
      ok: false,
      error: `${branch} shares no history with ${parentBranch} — wrong parent? (--onto)`,
    };
  }

  // File partition = the branch's own diff vs the anchor. Doubles as the
  // empty-slice guard (nothing to review → nothing to track).
  const diff = await gitRun(["diff", "--name-only", `${baseSha}..${branchRef}`]);
  const files = diff.stdout.trim().split("\n").filter(Boolean);
  if (diff.exitCode !== 0 || files.length === 0) {
    return { ok: false, error: `${branch} has no changes on top of ${parentBranch} (empty slice)` };
  }

  // Ensure the PR. A CLOSED PR is not adopted — gh happily opens a fresh one
  // on the same branch, and the closed PR wasn't tracking this work anyway.
  let prNumber: number;
  let prAction: "adopted" | "created";
  let title = opts.title ?? "";
  const existing = await viewPrInfo(branch);
  if (existing?.state === "MERGED") {
    return {
      ok: false,
      error: `PR #${existing.number} for ${branch} is already merged — nothing left to stack`,
    };
  }
  if (existing && existing.state === "OPEN") {
    prNumber = existing.number;
    prAction = "adopted";
    title = title || existing.title;
    onLog(`adopted existing PR #${existing.number}`);
  } else {
    if (!(await gitQuiet(["show-ref", "--verify", "--quiet", `refs/remotes/origin/${branch}`]))) {
      const push = await gitRun(["push", "-u", "origin", branch]);
      if (push.exitCode !== 0) {
        return { ok: false, error: `push ${branch}: ${(push.stderr || push.stdout).trim()}` };
      }
      onLog(`pushed ${branch}`);
    }
    title = title || titleFromBranch(branch);
    const pr = await createDraftPr({
      cwd: config.paths.mainClone,
      head: branch,
      base: parentBranch,
      title,
      body: `Stacked on \`${parentBranch}\`.`,
    });
    if (!pr.ok) return { ok: false, error: `pr create ${branch}: ${pr.error}` };
    prNumber = pr.number;
    prAction = "created";
    onLog(`opened draft PR #${pr.number} (base ${parentBranch})`);
  }

  const maxOrdinal = Math.max(0, ...manifest.slices.map((s) => s.ordinal));
  const ids = new Set(manifest.slices.map((s) => s.id));
  let n = maxOrdinal + 1;
  while (ids.has(`s${n}`)) n++;

  const slice: StackSlice = {
    id: `s${n}`,
    ordinal: maxOrdinal + 1,
    title: title || titleFromBranch(branch),
    branch,
    base: parentSlice ? parentSlice.id : config.branch.base,
    dependsOn: parentSlice ? [parentSlice.id] : [],
    files,
    pr: prNumber,
    status: "open",
    oversized: false,
    baseSha,
  };

  const next: StackManifest = { ...manifest, slices: [...manifest.slices, slice] };
  const v = validateStackManifest(next);
  if (!v.ok) {
    return { ok: false, error: `manifest invalid after add:\n  ${v.errors.join("\n  ")}` };
  }
  putStackManifest(v.manifest);

  // An adopted PR may target the wrong base (e.g. trunk, because `gh pr
  // create` defaulted there) — align it with the manifest like replay does.
  if (prAction === "adopted") await retargetIfNeeded(slice, parentBranch, onLog);

  log.info("added slice to stack", { stackId, slice: slice.id, branch, parentBranch, prNumber });
  return { ok: true, slice, parentBranch, prAction };
}

// ---------- backup pruning ----------

export type PruneBackupsResult = { deleted: string[]; kept: string[] };

/**
 * Delete restack backup branches (`backup/restack-*` and the retired stack
 * CLI's `backup/stack-sync-*`) older than `olderThanDays` (0 = all of them).
 * Backups exist to recover an in-flight conflict bail; once a slice replays
 * clean the engine prunes its own, but conflict leftovers and pre-pruning
 * history pile up — this is the manual sweep. `git branch -D` doesn't destroy
 * commits; everything stays reachable via the reflog. Refs under `backup/`
 * that don't match a known naming scheme are left alone.
 */
export async function pruneStackBackups(
  olderThanDays: number,
  onLog: Logger,
): Promise<PruneBackupsResult> {
  const r = await gitRun(["for-each-ref", "--format=%(refname:short)", "refs/heads/backup/"]);
  const deleted: string[] = [];
  const kept: string[] = [];
  if (r.exitCode !== 0) return { deleted, kept };
  const cutoff = Date.now() - olderThanDays * 86_400_000;
  for (const ref of r.stdout.split("\n").map((l) => l.trim()).filter(Boolean)) {
    if (backupBranchOwner(ref) === null) {
      kept.push(ref);
      continue;
    }
    const ts = backupTimestamp(ref);
    if (ts === null || ts > cutoff) {
      kept.push(ref);
      continue;
    }
    const del = await gitRun(["branch", "-D", ref]);
    if (del.exitCode === 0) {
      deleted.push(ref);
      onLog(`  deleted ${ref}`);
    } else {
      kept.push(ref);
      onLog(`  could not delete ${ref}: ${(del.stderr || del.stdout).trim()}`);
    }
  }
  return { deleted, kept };
}
