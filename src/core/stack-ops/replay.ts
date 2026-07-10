import { config } from "../config.ts";
import { firstSha, gitQuiet, gitRun, revParse } from "../git.ts";
import { rebaseInProgress, restackEngine } from "./engine.ts";
import { isTrunkBase, resolveParentBranch, topoSortSlices } from "../stack-layout.ts";
import { getStackManifest, updateStackSlice, type StackSlice } from "../wtstate.ts";
import { fetchOrigin, listWorktrees, worktreeHasTrackedChanges } from "../worktree.ts";
import { acquireStackLock, log, retargetIfNeeded, STACK_BUSY, type Logger } from "./shared.ts";
import { reconcileStack } from "./reconcile.ts";

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
  const handle = await acquireStackLock("replay");
  if (!handle) {
    return { ok: false, conflict: false, error: STACK_BUSY };
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

  // Freshen origin and advance the local trunk branch before replaying. A
  // failed fetch would silently leave stale refs and replay every slice onto
  // an outdated base, so bail.
  try {
    await fetchOrigin();
  } catch (err) {
    return {
      ok: false,
      conflict: false,
      error: `${err instanceof Error ? err.message : String(err)}; refusing to replay onto possibly-stale refs`,
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

/**
 * The parent-tip SHA a slice currently sits on (the `rebase --onto` anchor):
 * the *descendant-most* of the stored `baseSha` and the live merge-base of the
 * slice with its current parent ref — computed before any replay so siblings
 * haven't moved.
 *
 * The stored `baseSha` is the squash-safe cut point (the parent tip this slice's
 * commits were last based on). A conflict bail hands resolution to a human, who
 * rebases the slice by hand and force-pushes WITHOUT updating the manifest, so
 * the stored anchor goes stale. Replaying off a stale anchor re-applies the
 * parent's already-present commits onto themselves, a bogus conflict on an
 * already-correct slice.
 *
 * Two ways the stored anchor goes stale, and why a bare `--is-ancestor` guard is
 * not enough (it bit eng-5182 twice, then eng-5244 again):
 *
 *  1. Hand-rebased OFF the anchor entirely → `baseSha` is no longer an ancestor
 *     of the branch. Caught by the ancestor check; fall back to the merge-base.
 *  2. Hand-rebased onto NEWER trunk that itself descends from `baseSha` (main
 *     advanced mid-restack, or a fix-then-rebase-onto-fresh-main). `baseSha` is
 *     STILL an ancestor of the branch, so the ancestor check passes — but the
 *     real fork point has moved up to the live merge-base, which sits ABOVE
 *     `baseSha`. Cutting at the old anchor replays all of trunk's squashed
 *     history. The naive guard misses this.
 *
 * So when BOTH `baseSha` and the live merge-base are ancestors of the branch,
 * pick whichever is the descendant: the live merge-base wins after a rebase onto
 * newer trunk (case 2), `baseSha` wins in the healthy squash-merge case (its
 * commits sit ABOVE the pre-squash merge-base, so it stays the cut point that
 * excludes the squash-merged parent). Self-healing, no manual bookkeeping.
 */
export async function resolveAnchor(
  slice: StackSlice,
  byId: Map<string, StackSlice>,
  trunk: string,
  cwd: string,
): Promise<string | null> {
  const parentRef = currentParentRef(slice, byId, trunk);
  const mb = await gitRun(["merge-base", slice.branch, parentRef], cwd);
  const liveAnchor = mb.exitCode === 0 && mb.stdout.trim() ? mb.stdout.trim() : null;

  if (slice.baseSha) {
    const storedIsAncestor = await gitQuiet(
      ["merge-base", "--is-ancestor", slice.baseSha, slice.branch],
      cwd,
    );
    if (storedIsAncestor) {
      if (!liveAnchor) return slice.baseSha;
      // Both anchor the branch; use the more recent cut point. `baseSha` below
      // the live merge-base means the branch was rebased onto newer trunk —
      // the live merge-base is the true fork point (case 2). Otherwise the live
      // merge-base is at/below `baseSha` (squash case), so `baseSha` stands.
      const baseShaBelowLive = await gitQuiet(
        ["merge-base", "--is-ancestor", slice.baseSha, liveAnchor],
        cwd,
      );
      return baseShaBelowLive ? liveAnchor : slice.baseSha;
    }
  }
  return liveAnchor;
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
  if (isTrunkBase(slice)) return revParse(`origin/${trunk}`, cwd);
  const sibling = byId.get(slice.base);
  if (sibling) {
    const replayed = newTipById.get(sibling.id);
    return replayed ?? revParse(sibling.branch, cwd);
  }
  // External parent branch: prefer the local checkout, fall back to origin.
  return firstSha(cwd, [slice.base, `origin/${slice.base}`]);
}
