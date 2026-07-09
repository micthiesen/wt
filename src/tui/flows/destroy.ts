/**
 * Destroy / clean / restack flows, extracted from `app.tsx`. Pure
 * functions over an explicit context object — `makeDestroyFlows` is
 * called per render inside `App` with the current rows + action
 * helpers, so the returned closures always see fresh state (same
 * semantics as when these lived inline).
 */
import { actionRegistry } from "../../core/actions.ts";
import { config } from "../../core/config.ts";
import { spawnBackgroundRemove } from "../../core/lifecycle.ts";
import { lockLabel, lockStatus } from "../../core/locks.ts";
import { createLogger } from "../../core/logger.ts";
import { removeShellLog } from "../../core/shell-tail.ts";
import { rebaseStack, reconcileStack } from "../../core/stack-ops.ts";
import { killAllSessionsFor } from "../../core/tmux.ts";
import { findStackIdByBranch } from "../../core/wtstate.ts";

import { isCleanCandidate } from "../app-helpers.ts";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";
import { theme } from "../theme.ts";

const appLog = createLogger("[app]");

export type DestroyFlowsCtx = {
  rows: readonly WorktreeRow[];
  /** Currently-selected row (for `doReplayStack`'s stack resolution). */
  current: WorktreeRow | undefined;
  toast: (message: string, color?: string, ms?: number) => void;
  /** Idempotently mark a slug archived (see `useWtActions.archive`). */
  archive: (slug: string) => void;
  refreshTmuxSessions: () => Promise<void>;
  invalidateWorktree: (slug: string) => Promise<void>;
  refreshAll: () => Promise<void>;
  refreshGithub: () => Promise<void>;
  /**
   * Re-entry guard for `R` while a rebase is in flight (the engine
   * flock is the real lock; this just avoids spamming it from the UI).
   */
  restackBusyRef: { current: boolean };
};

export function makeDestroyFlows(ctx: DestroyFlowsCtx) {
  const {
    rows,
    toast,
    archive,
    refreshTmuxSessions,
    invalidateWorktree,
    refreshAll,
    refreshGithub,
    restackBusyRef,
  } = ctx;

  async function doRemove(
    slug: string,
    opts: { force?: boolean } = {},
  ): Promise<void> {
    const log = createLogger(slug);
    const row = rows.find((r) => r.wt.slug === slug);
    if (!row) return;
    // Authoritative busy check via on-disk flock. Beats relying on the
    // cached lock query, which can still read "clean" for ~600ms after a
    // prior `d` dispatched its background destroy.
    const lock = lockStatus(slug);
    if (lock) {
      const label = lockLabel(lock);
      log.event.warn(`refused: ${label}`);
      toast(`${slug} is ${label}`, theme.warn, 2000);
      return;
    }
    const force = opts.force ?? false;
    if (!force) {
      if ((row.fields.dirty.data?.length ?? 0) > 0) {
        log.event.err("refused: uncommitted changes, press d again to force");
        toast(`${slug} has uncommitted changes`, theme.err, 3000);
        return;
      }
      const unpushed = row.fields.sync.data?.remote?.ahead ?? 0;
      if (unpushed > 0) {
        const plural = unpushed === 1 ? "" : "s";
        log.event.err(
          `refused: ${unpushed} unpushed commit${plural}, press d again to force`,
        );
        toast(`${slug} has ${unpushed} unpushed commit${plural}`, theme.err, 3000);
        return;
      }
    } else {
      log.event.warn("force destroy: skipping dirty + unpushed guards");
    }
    // Tuck the row into the archived section for the duration of the
    // destroy — keeps the active list uncluttered while tail output
    // spills into the activity pane. The archive entry intentionally
    // outlives the destroy: removeWorktree leaves archive.json alone so
    // the row keeps its archived styling until it actually disappears
    // from the worktree list (driven by the lock-released → invalidate
    // worktrees trigger in useWorktreeRows). Stale entries are reaped
    // at next startup; re-creating the same slug clears the entry via
    // createWorktree.
    archive(slug);
    // Mark any in-flight action as killed in the registry first, so
    // the activity pane reads "killed" rather than the "failed" the
    // wrapper's exit code would otherwise produce. Has to happen
    // before killAllSessionsFor below — once tmux drops the session
    // out from under the wrapper there's no way for the registry to
    // distinguish "user destroyed worktree" from "wrapper crashed".
    // kill() commits the "killed" status synchronously before its async
    // tmux teardown, so the status flip lands before killAllSessionsFor
    // below even though we don't await here.
    void actionRegistry.kill(slug);
    // Tear down any interactive sessions (claude, diff, shell) BEFORE
    // the worktree removal starts. Their cwds are inside the worktree;
    // letting the remove race against a live tmux child can leave it
    // writing into a half-deleted directory. killAllSessionsFor is
    // idempotent and fast (just SIGHUPs the tmux session daemons).
    // Awaited so spawnBackgroundRemove only starts once they're gone.
    try {
      await killAllSessionsFor(slug);
      void refreshTmuxSessions();
    } catch (err) {
      log.warn("kill session before remove failed", {
        err: err instanceof Error ? err.message : String(err),
      });
      // Don't block the destroy on a kill failure — worst case the
      // session is already dead, or it'll get reaped on next startup.
    }
    // Drop the shell-tail log now that the session is gone — the
    // startup reap would catch it eventually, but cleaning up at the
    // source keeps the cache dir tidy without waiting for a restart.
    removeShellLog(slug);
    spawnBackgroundRemove(slug, {
      force,
      destroyStage: row.fields.deploy.data ?? false,
      deleteBranch: true,
    });
    log.event.info(`dispatched destroy${force ? " (force)" : ""}`);
    toast(`dispatched destroy of ${slug}`, theme.info);
    setTimeout(() => void invalidateWorktree(slug), 600);
  }

  async function doClean(): Promise<void> {
    const candidates = rows.filter((r) => isCleanCandidate(r));
    if (candidates.length === 0) {
      appLog.event.dim("clean: nothing to clean");
      toast("nothing to clean", theme.fgDim, 1500);
      return;
    }
    await doCleanRows(candidates);
  }

  /**
   * Scoped clean for the automations engine: destroy just the listed
   * slugs, re-filtered through `isCleanCandidate` against CURRENT rows
   * so a fire computed a render ago can't destroy something that
   * un-merged in between. Silently no-ops on an empty survivor set.
   */
  async function doCleanSlugs(slugs: readonly string[]): Promise<void> {
    const want = new Set(slugs);
    const candidates = rows.filter(
      (r) => want.has(r.wt.slug) && isCleanCandidate(r),
    );
    if (candidates.length === 0) return;
    await doCleanRows(candidates);
  }

  async function doCleanRows(candidates: readonly WorktreeRow[]): Promise<void> {
    appLog.event.info(
      `clean: dispatching ${candidates.length} destroy${candidates.length === 1 ? "" : "s"}`,
    );
    // Kill every candidate's tmux sessions (every kind) before
    // dispatching any remove — same rationale as `doRemove`: don't
    // let the remove race against a live child with cwd inside the
    // worktree. Done in parallel since each kill is independent.
    // Notify the action registry first (synchronous, fast) so the
    // activity pane reads "killed" rather than the "failed" the
    // wrapper's exit code would otherwise produce.
    for (const row of candidates) void actionRegistry.kill(row.wt.slug);
    await Promise.allSettled(
      candidates.map((row) => killAllSessionsFor(row.wt.slug)),
    );
    void refreshTmuxSessions();
    for (const row of candidates) {
      archive(row.wt.slug);
      removeShellLog(row.wt.slug);
      spawnBackgroundRemove(row.wt.slug, {
        force: false,
        destroyStage: row.fields.deploy.data ?? false,
        deleteBranch: true,
      });
      createLogger(row.wt.slug).event.info("dispatched destroy (clean)");
    }
    // Cleaning a merged stack slice orphans its children: their recorded
    // base is the branch we just deleted, so the list would diff/sync
    // against a dead ref and surface a raw rev-parse error. Reconcile the
    // affected manifests now — pure bookkeeping (mark merged, reparent the
    // orphans onto trunk), no rebase/push — so the children re-root and the
    // error clears. The actual replay (rebasing commits off the squashed
    // parent) stays an explicit `/restack`.
    void reconcileCleanedStacks(candidates.map((r) => r.wt.branch));
    setTimeout(() => void refreshAll(), 600);
  }

  /**
   * Manifest-only reconcile of every stack a just-cleaned branch belonged
   * to. Local + idempotent (no git history rewrite, no force-push), so it's
   * safe to run automatically off the `c` keystroke; it probes live PR
   * state to mark merged slices and reparents their orphaned children onto
   * trunk. Best-effort: a failure logs and leaves the manifest as-is for an
   * explicit `/restack` to repair.
   */
  async function reconcileCleanedStacks(branches: readonly string[]): Promise<void> {
    const stackIds = new Set<string>();
    for (const branch of branches) {
      const id = findStackIdByBranch(branch);
      if (id) stackIds.add(id);
    }
    if (stackIds.size === 0) return;
    for (const stackId of stackIds) {
      try {
        await reconcileStack(stackId, config.branch.base, (line) =>
          appLog.event.dim(`reconcile ${stackId}: ${line}`),
        );
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        appLog.event.warn(`reconcile ${stackId} failed: ${msg}`);
      }
    }
    void refreshAll();
  }

  /**
   * `R` — the algorithmic fast path for restacking. Resolves the stack the
   * selected worktree belongs to (its branch → `findStackIdByBranch`) and
   * runs the whole stack through `rebaseStack`: fetch, reconcile, then
   * squash-safe replay of every slice onto its parent (already-based slices
   * are cheap no-ops). No model input — it streams progress to the activity
   * pane. On a clean conflict bail it stops and points at `/restack`, which
   * owns the judgment the engine can't do. Whole-stack on purpose: restack is
   * a coherence operation, and the worktree only selects *which* stack.
   */
  async function doReplayStack(): Promise<void> {
    const { current } = ctx;
    if (!current?.wt.branch) {
      toast("select a stack slice first", theme.warn, 2000);
      return;
    }
    const stackId = findStackIdByBranch(current.wt.branch);
    if (!stackId) {
      toast("not a stack slice", theme.warn, 2000);
      return;
    }
    await doRestackStack(stackId);
  }

  /**
   * The stack-resolved half of `doReplayStack`, shared with the
   * automations engine (`builtin:restack` dispatches here after
   * pre-cleaning the merged slices via `doCleanSlugs`). Returns whether
   * the replay landed clean so the caller can decide how loudly to
   * report; the conflict-bail escalation to /restack stays manual
   * either way.
   */
  async function doRestackStack(stackId: string): Promise<boolean> {
    if (restackBusyRef.current) {
      toast("restack already running", theme.warn, 2000);
      return false;
    }
    restackBusyRef.current = true;
    let clean = false;
    appLog.event.info(`restack ${stackId}: fetch + reconcile + replay`);
    try {
      const res = await rebaseStack(stackId, {}, (line) =>
        appLog.event.dim(`restack ${stackId}: ${line}`),
      );
      if (res.ok) {
        clean = true;
        appLog.event.ok(`restacked ${stackId}: ${res.output}`);
        toast(`restacked ${stackId}`, theme.ok, 2500);
      } else if (res.conflict) {
        const where = res.failedBranch ? ` on ${res.failedBranch}` : "";
        const backup = res.backupBranch ? ` (backup ${res.backupBranch})` : "";
        appLog.event.warn(
          `restack ${stackId}: conflict${where}${backup} — run /restack to resolve`,
        );
        toast(`conflict${where} — run /restack`, theme.warn, 6000);
      } else {
        appLog.event.err(`restack ${stackId} failed: ${res.error}`);
        toast(`restack failed: ${res.error}`, theme.err, 6000);
      }
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      appLog.event.err(`restack ${stackId} crashed: ${msg}`);
      toast(`restack crashed: ${msg}`, theme.err, 6000);
    } finally {
      restackBusyRef.current = false;
    }
    // PR bases shift when slices move, so refresh the github query (keyed by
    // branch list, not slug) alongside the worktree state.
    void refreshGithub();
    void refreshAll();
    return clean;
  }

  return { doRemove, doClean, doCleanSlugs, doReplayStack, doRestackStack };
}
