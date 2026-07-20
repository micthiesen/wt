import { config } from "../config.ts";
import { firstSha, gitQuiet, gitRun, rebaseInProgress, revParse } from "../git.ts";
import { restackEngine } from "./engine.ts";
import { resolveChain, type ChainStep, type RestackChain } from "./chain.ts";
import { advanceBaseAnchor } from "../wtstate.ts";
import { fetchOrigin, worktreeHasTrackedChanges } from "../worktree.ts";
import { lockChain, log, retargetIfNeeded, STACK_BUSY, type Logger } from "./shared.ts";
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
 * The one-shot restack convenience: reconcile the chain's fork-base
 * records against landed PRs, then replay every member onto its
 * (possibly rewritten) parent. `reconcileStack` and `replayStack` are
 * exposed separately so `/restack` can drive them step-by-step around a
 * conflict (reconcile once, replay → resolve → replay again).
 *
 * The membership is snapshotted BEFORE the reconcile: when the named
 * branch's own PR merged (the common bottom-up landing), reconcile
 * reparents its dependents away from it, after which that branch alone
 * no longer resolves the work that needs replaying — and a landed root
 * can split its children into several independent chains. So the replay
 * walks every distinct surviving chain the old membership maps to,
 * instead of trusting the original name to still be the handle.
 *
 * A member reconcile observed LANDED (a merged parent that is itself
 * still a live, uncleaned worktree) is dropped from the replay set:
 * resolving its now-childless chain would replay its squash-merged
 * commits onto trunk and force-push, resurrecting the landed branch (or
 * spuriously conflicting). Cleaning it (`c`) is the verb for a landed
 * member; the engine leaves it alone. This is what makes pressing `R` on
 * a surviving sibling safe while a merged member is still on disk, and
 * makes the automation pre-clean's dispatch-then-restack ordering benign.
 */
export async function rebaseStack(
  branch: string,
  opts: RebaseOptions,
  onLog: Logger,
): Promise<RebaseResult> {
  const trunk = opts.onto ?? config.branch.base;
  const pre = await resolveChain(branch);
  const landed = await reconcileStack(branch, trunk, onLog);
  const candidates = (pre ? pre.steps.map((s) => s.branch) : [branch]).filter(
    (b) => !landed.has(b),
  );
  const replayedRoots = new Set<string>();
  const outputs: string[] = [];
  for (const candidate of candidates) {
    const chain = await resolveChain(candidate);
    if (!chain || replayedRoots.has(chain.root)) continue;
    replayedRoots.add(chain.root);
    const res = await replayStack(candidate, { onto: trunk }, onLog);
    if (!res.ok) return res;
    outputs.push(res.output);
  }
  if (replayedRoots.size === 0) {
    // Distinguish "the branch is gone" from "every member has landed and
    // was skipped" — the latter wants `c`, not a puzzled "no worktree".
    const allLanded = candidates.length === 0 && landed.size > 0;
    return {
      ok: false,
      conflict: false,
      error: allLanded
        ? `every member of ${branch} has landed — clean them with c`
        : `${branch} has no live worktree to restack`,
    };
  }
  return { ok: true, output: outputs.join("; ") };
}

/**
 * Replay every member of the stack containing `branch` onto its parent,
 * squash-safe, parents before children: rebase each member's own
 * commits in its own worktree, force-push, and retarget its PR base to
 * match the recorded parent. Bails clean on the first conflict, naming
 * the branch + the backup ref the engine left — wt never auto-resolves.
 * Pure git + gh; does NOT reconcile (run `reconcileStack` first for a
 * post-merge restack). Serialized against other writers on the same
 * worktrees by the members' per-slug flocks (see `lockChain`); disjoint
 * chains replay concurrently.
 */
export async function replayStack(
  branch: string,
  opts: RebaseOptions,
  onLog: Logger,
): Promise<RebaseResult> {
  const locked = await lockChain(branch, "replay");
  if (locked.status === "busy") {
    return { ok: false, conflict: false, error: STACK_BUSY };
  }
  if (locked.status === "gone") {
    return {
      ok: false,
      conflict: false,
      error: `${branch} has no live worktree to restack`,
    };
  }
  try {
    return await replayStackLocked(locked.chain, opts, onLog);
  } finally {
    for (const h of locked.handles) h.release();
  }
}

async function replayStackLocked(
  // Resolved under the chain's locks by `lockChain` — post-reconcile
  // reparents are already visible, parents before children so each
  // parent replays before its dependents.
  chain: RestackChain,
  opts: RebaseOptions,
  onLog: Logger,
): Promise<RebaseResult> {
  const trunk = opts.onto ?? config.branch.base;

  // Every member replays IN ITS OWN WORKTREE (HEAD rebases in place);
  // refuse any dirty one up front — a rebase would clobber. Only
  // TRACKED changes block: untracked files ride through a rebase safely
  // (git refuses cleanly if one would be overwritten).
  for (const s of chain.steps) {
    if (await worktreeHasTrackedChanges(s.worktreePath)) {
      return {
        ok: false,
        conflict: false,
        error: `worktree ${s.worktreePath} (${s.branch}) has uncommitted changes to tracked files — commit or stash before restacking`,
      };
    }
    // A worktree left mid-rebase by an earlier interrupted run can read
    // clean via `git status --porcelain` (no unmerged paths), so the
    // dirty check alone misses it. Replaying into it would make the
    // engine abort and silently discard that in-flight state — refuse
    // up front instead.
    if (await rebaseInProgress(s.worktreePath)) {
      return {
        ok: false,
        conflict: false,
        error: `worktree ${s.worktreePath} (${s.branch}) is mid-rebase from an unfinished run — finish or \`git rebase --abort\` it there before restacking`,
      };
    }
  }

  // Freshen origin before replaying. A failed fetch would silently
  // leave stale refs and replay every member onto an outdated base.
  try {
    await fetchOrigin();
  } catch (err) {
    return {
      ok: false,
      conflict: false,
      error: `${err instanceof Error ? err.message : String(err)}; refusing to replay onto possibly-stale refs`,
    };
  }

  // Pass 1: resolve each member's anchor (the old parent tip its
  // commits sit on) BEFORE any rewrite — so the merge-base fallback
  // sees pre-replay tips, AND so an unresolvable anchor fails the whole
  // run before a single branch has been pushed.
  const anchorByBranch = new Map<string, string>();
  for (const s of chain.steps) {
    const anchor = await resolveAnchor(s, parentRefOf(s, trunk), s.worktreePath);
    if (!anchor) {
      return {
        ok: false,
        conflict: false,
        error: `could not resolve a replay anchor for ${s.branch} (no recorded base sha and no merge-base with ${parentRefOf(s, trunk)})`,
      };
    }
    anchorByBranch.set(s.branch, anchor);
  }

  // Pass 2: replay top-down, threading each member's new tip to its
  // children.
  const newTipByBranch = new Map<string, string>();
  let replayed = 0;
  for (const s of chain.steps) {
    const anchor = anchorByBranch.get(s.branch)!;
    const newBase = await resolveNewBaseSha(s, trunk, newTipByBranch, s.worktreePath);
    if (!newBase) {
      return {
        ok: false,
        conflict: false,
        error: `could not resolve the new base for ${s.branch} (parent ${s.parentBranch ?? trunk})`,
      };
    }

    onLog(`replay ${s.branch}`);
    const out = await restackEngine.replayStep(
      { branch: s.branch, worktreePath: s.worktreePath, anchor, newBase },
      onLog,
    );
    if (!out.ok) {
      // Persist the failure to the daily app log — the engine only streams to
      // the console `onLog`, so a replay run from the CLI would otherwise leave
      // nothing to diagnose after the fact.
      log.warn("replay failed", {
        stack: chain.root,
        branch: s.branch,
        conflict: out.conflict,
        worktree: s.worktreePath,
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
    newTipByBranch.set(s.branch, out.newTip);
    // Advance the stored anchor to the parent tip we just landed on, so
    // the next restack is a cheap no-op when nothing has moved. A
    // trunk-based member that never had a record stays record-free —
    // its live merge-base with trunk IS the anchor. Compare-and-set: a
    // background destroy may have reparented this record mid-replay
    // (its writer isn't under the stack lock); when the record moved,
    // leave it alone rather than clobbering the reparent with the
    // stale pre-destroy parent.
    if (s.parentBranch !== null || s.hasRecord) {
      const advanced = advanceBaseAnchor(s.slug, s.parentBranch ?? trunk, out.newBaseSha);
      if (!advanced) {
        onLog(`  ${s.branch}: base record changed mid-replay — left for the next reconcile`);
      }
    }

    // Keep the PR base aligned with the recorded parent when the member
    // actually moved (a parent that landed/rewrote shifts the child onto
    // a new base) OR when the engine synced a stale remote — a
    // hand-resolved conflict may have changed the parent too.
    if (out.moved) replayed++;
    if (out.moved || out.pushed) {
      await retargetIfNeeded(s.branch, s.parentBranch ?? trunk, onLog);
    }
  }

  return { ok: true, output: `replayed ${replayed}/${chain.steps.length} worktree(s)` };
}

/** The ref naming a member's parent tip as it stands now (pre-replay). */
function parentRefOf(step: ChainStep, trunk: string): string {
  return step.parentBranch ?? `origin/${trunk}`;
}

/**
 * The parent-tip SHA a member currently sits on (the `rebase --onto`
 * anchor): the *descendant-most* of the stored `baseSha` and the live
 * merge-base of the branch with its current parent ref — computed before
 * any replay so parents haven't moved.
 *
 * The stored `baseSha` is the squash-safe cut point (the parent tip this
 * branch's commits were last based on). A conflict bail hands resolution to a
 * human, who rebases the branch by hand and force-pushes WITHOUT updating the
 * record, so the stored anchor goes stale. Replaying off a stale anchor
 * re-applies the parent's already-present commits onto themselves, a bogus
 * conflict on an already-correct branch.
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
  step: Pick<ChainStep, "branch" | "baseSha">,
  parentRef: string,
  cwd: string,
): Promise<string | null> {
  const mb = await gitRun(["merge-base", step.branch, parentRef], cwd);
  const liveAnchor = mb.exitCode === 0 && mb.stdout.trim() ? mb.stdout.trim() : null;

  if (step.baseSha) {
    const storedIsAncestor = await gitQuiet(
      ["merge-base", "--is-ancestor", step.baseSha, step.branch],
      cwd,
    );
    if (storedIsAncestor) {
      if (!liveAnchor) return step.baseSha;
      // Both anchor the branch; use the more recent cut point. `baseSha` below
      // the live merge-base means the branch was rebased onto newer trunk —
      // the live merge-base is the true fork point (case 2). Otherwise the live
      // merge-base is at/below `baseSha` (squash case), so `baseSha` stands.
      const baseShaBelowLive = await gitQuiet(
        ["merge-base", "--is-ancestor", step.baseSha, liveAnchor],
        cwd,
      );
      return baseShaBelowLive ? liveAnchor : step.baseSha;
    }
  }
  return liveAnchor;
}

/**
 * The SHA to rebase a member ONTO this run: trunk's freshly-fetched tip,
 * an in-chain parent's just-replayed tip, or an external parent branch's
 * live tip (a branch stacked on a parent with no live worktree).
 */
async function resolveNewBaseSha(
  step: ChainStep,
  trunk: string,
  newTipByBranch: Map<string, string>,
  cwd: string,
): Promise<string | null> {
  if (step.parentBranch === null) return revParse(`origin/${trunk}`, cwd);
  const replayed = newTipByBranch.get(step.parentBranch);
  if (replayed) return replayed;
  // Parent outside this run (external ref, or the standalone fallback):
  // prefer the local checkout, fall back to origin.
  return firstSha(cwd, [step.parentBranch, `origin/${step.parentBranch}`]);
}
