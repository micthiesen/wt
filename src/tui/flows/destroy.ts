/**
 * Destroy / clean / restack flows, extracted from `app.tsx`. Pure
 * functions over an explicit context object — `makeDestroyFlows` is
 * called per render inside `App` with the current rows + action
 * helpers, so the returned closures always see fresh state (same
 * semantics as when these lived inline).
 */
import { actionRegistry } from "../../core/actions.ts";
import { spawnBackgroundRemove } from "../../core/lifecycle.ts";
import { lockLabel, lockStatus } from "../../core/locks.ts";
import { createLogger } from "../../core/logger.ts";
import { removeShellLog } from "../../core/shell-tail.ts";
import { rebaseStack } from "../../core/stack-ops.ts";
import { killAllSessionsFor } from "../../core/tmux.ts";
import {
  recordRemovedWorktrees,
  type RemovedWorktree,
} from "../../core/wtstate.ts";

import { isCleanCandidate } from "../app-helpers.ts";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";
import { theme } from "../theme.ts";

const appLog = createLogger("[app]");

/**
 * Rich removed-history snapshot taken at destroy DISPATCH, while the
 * row's PR + AI title are still in hand — `removeWorktree` later
 * confirms with a minimal upsert that preserves these fields. A
 * slug-derived title is omitted (the slug is already on the entry).
 */
function removedSnapshot(row: WorktreeRow): RemovedWorktree {
  return {
    slug: row.wt.slug,
    branch: row.wt.branch,
    removedAt: new Date().toISOString(),
    ...(row.titleSource !== "slug" ? { title: row.title } : {}),
    ...(row.pr
      ? { prNumber: row.pr.number, prUrl: row.pr.url, prState: row.pr.state }
      : {}),
  };
}

/**
 * Best-effort history write — a state-file IO failure must never block
 * a destroy the user already confirmed.
 */
function recordRemovedSnapshots(rows: readonly WorktreeRow[]): void {
  try {
    recordRemovedWorktrees(rows.map(removedSnapshot));
  } catch (err) {
    appLog.warn("could not record removed-worktree history", {
      err: err instanceof Error ? err.message : String(err),
    });
  }
}

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
      // Unknown ≠ clean: both fields default to "no data" while their
      // queries load (or after an error), and the destroy deletes the
      // branch with -D — treating that window as clean could drop
      // uncommitted files or unpushed commits. Force skips this, and the
      // `d` prompt offers the force variant whenever state is unknown.
      if (
        row.fields.dirty.data === undefined ||
        row.fields.sync.data === undefined
      ) {
        log.event.warn("refused: dirty/unpushed state still loading, retry in a moment");
        toast(`${slug} state still loading, retry in a moment`, theme.warn, 2500);
        return;
      }
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
    recordRemovedSnapshots([row]);
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
    recordRemovedSnapshots(candidates);
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
    // Cleaning a merged stack member reparents its children's fork-base
    // records inside the background remove itself (the branch delete
    // triggers `reparentBaseReferences`, anchors preserved), so no
    // TUI-side bookkeeping is needed here. The actual replay (rebasing
    // commits off the squashed parent) stays an explicit `R`/`/restack`.
    setTimeout(() => void refreshAll(), 600);
  }

  /**
   * `R` — the algorithmic fast path for getting the selected worktree
   * current, whatever its shape. Runs the chain containing it through
   * `rebaseStack`: fetch, reconcile the fork-base records against
   * landed PRs, then squash-safe replay of every member onto its
   * parent (already-based members are cheap no-ops). A stack member
   * restacks the WHOLE stack (restack is a coherence operation; the
   * worktree only selects which stack); a standalone worktree is a
   * one-member chain that rebases onto its recorded base or trunk with
   * the same engine. No model input — it streams progress to the
   * activity pane. On a clean conflict bail it stops and points at
   * `/restack`, which owns the judgment the engine can't do.
   */
  async function doReplayStack(): Promise<void> {
    const { current } = ctx;
    if (!current?.wt.branch) {
      toast("select a worktree first", theme.warn, 2000);
      return;
    }
    // A landed-but-not-yet-cleaned row has nothing useful to rebase —
    // replaying it onto trunk drops its already-merged commits and
    // force-pushes an empty diff to the PR branch. `c` is the verb for
    // it. (A merged member elsewhere in a stack is fine: reconcile
    // handles it when R is pressed on a surviving member.)
    if (isCleanCandidate(current)) {
      toast("branch already landed — clean it (c) instead of rebasing", theme.warn, 3000);
      return;
    }
    await doRestackStack(current.wt.branch);
  }

  /**
   * The branch-resolved half of `doReplayStack`, shared with the
   * automations engine (`builtin:restack` dispatches here after
   * pre-cleaning the merged members via `doCleanSlugs`). `stackId` is
   * any branch in the target stack (the engine resolves the whole
   * chain from it). "busy" means the app-wide restack mutex was held
   * (a manual `R` or another auto-restack) and NOTHING ran — the
   * automations engine un-consumes the fire on that outcome instead of
   * recording a restack that never happened. "clean" / "failed" report
   * the replay itself; the conflict-bail escalation to /restack stays
   * manual either way.
   */
  async function doRestackStack(
    stackId: string,
  ): Promise<"clean" | "failed" | "busy"> {
    if (restackBusyRef.current) {
      toast("restack already running", theme.warn, 2000);
      return "busy";
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
    return clean ? "clean" : "failed";
  }

  /**
   * Non-mutating peek at the app-wide restack mutex, for the
   * automations engine's dispatch gate: a restack intent stays queued
   * while a manual `R` (or another auto-restack) holds the engine,
   * BEFORE it pre-cleans anything — cleaning first and then finding
   * the mutex held would strand the stack with its trigger condition
   * already consumed by the clean.
   */
  function isRestackBusy(): boolean {
    return restackBusyRef.current;
  }

  return { doRemove, doClean, doCleanSlugs, doReplayStack, doRestackStack, isRestackBusy };
}
