/**
 * Destroy / clean / restack flows, extracted from `app.tsx`. Pure
 * functions over an explicit context object — `makeDestroyFlows` is
 * called per render inside `App` with the current rows + action
 * helpers, so the returned closures always see fresh state (same
 * semantics as when these lived inline).
 */
import { existsSync } from "node:fs";

import { actionRegistry } from "../../core/actions.ts";
import { config } from "../../core/config.ts";
import { getHarness, type HarnessId } from "../../core/harness/index.ts";
import { spawnBackgroundRemove } from "../../core/lifecycle.ts";
import { lockLabel, lockStatus } from "../../core/locks.ts";
import { createLogger } from "../../core/logger.ts";
import { runRemoteWt } from "../../core/remote.ts";
import { removeShellLog } from "../../core/shell-tail.ts";
import { rebaseStack, STACK_BUSY } from "../../core/stack-ops.ts";
import { injectIntoSession, killAllSessionsFor } from "../../core/tmux.ts";
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
  optimisticRemoveRemoteWorktree: (
    slug: string,
    run: () => Promise<void>,
  ) => Promise<void>;
  /**
   * Re-entry guard for `R`: the set of chains (stack id or standalone
   * branch) with a restack in flight. Same-chain re-presses are refused;
   * different chains run concurrently (the engine's per-slug flocks are
   * the real locks; this just avoids spamming them from the UI).
   */
  restackBusyRef: { current: Set<string> };
  /** Shift+TAB-selected primary harness — the conflict handoff injects
   *  the restack skill into this harness's session, same as a
   *  session-target action would. */
  primaryHarness: HarnessId;
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
    optimisticRemoveRemoteWorktree,
    restackBusyRef,
    primaryHarness,
  } = ctx;

  async function doRemoteRemove(
    slug: string,
    opts: { force?: boolean } = {},
  ): Promise<void> {
    const remote = config.remote;
    const log = createLogger(`[remote:${remote?.label ?? "remote"}]`);
    if (!remote) {
      toast("[remote] is not configured", theme.warn, 2200);
      return;
    }

    const force = opts.force ?? false;
    const args = [
      "rm",
      slug,
      "--yes",
      "--no-destroy-stage",
      "--delete-branch",
      ...(force ? ["--force"] : []),
    ];
    log.event.info(`removing ${slug}${force ? " (force)" : ""}`);
    try {
      await optimisticRemoveRemoteWorktree(slug, async () => {
        const code = await runRemoteWt(remote, args, {
          onLine: (line) => log.event.dim(line),
        });
        if (code !== 0) throw new Error(`remove failed (exit ${code})`);
      });
      log.event.ok(`removed ${slug} from ${remote.label}`);
      toast(`removed ${slug} from ${remote.label}`, theme.ok, 2200);
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      log.event.err(message);
      toast(`remote remove failed: ${message}`, theme.err, 3500);
    }
  }

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

  async function doCleanRows(input: readonly WorktreeRow[]): Promise<void> {
    // Drop any row whose on-disk flock is already held — a prior `d`/`c`
    // or an automation's clean already has a detached remove in flight on
    // it. `doRemove` makes this authoritative check per-row; `doClean`
    // fans out and must too, or a `c` racing a just-dispatched `d` (both
    // see the same clean candidate before the cache reflects the lock)
    // double-spawns `wt _destroy` on one slug. The loser only fails
    // because it loses the flock race inside `removeWorktree` — skipping
    // here makes the guard intentional instead of luck.
    const candidates = input.filter((r) => {
      const lock = lockStatus(r.wt.slug);
      if (lock) {
        createLogger(r.wt.slug).event.dim(`clean: skip — already ${lockLabel(lock)}`);
        return false;
      }
      return true;
    });
    if (candidates.length === 0) return;
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
   * the same engine. Algorithmic while it can be — it streams progress
   * to the activity pane — and on a conflict bail it hands the failing
   * worktree to the LLM automatically (the restack skill, injected into
   * its session), which owns the judgment the engine can't do.
   */
  async function doReplayStack(): Promise<void> {
    const { current } = ctx;
    if (!current?.wt.branch) {
      toast("select a worktree first", theme.warn, 2000);
      return;
    }
    // A row that's already being torn down must not be restacked. A
    // clean (`c`) or destroy (`d`) archives the row and dispatches a
    // detached background remove that deletes the worktree + branch;
    // `isCleanCandidate` returns false the instant `archived` flips, so
    // the landed-guard below stops covering it. Restacking here races
    // the removal — the replay force-pushes an empty diff to a landed
    // branch, and a conflict bail cold-starts a session in the worktree
    // being deleted (the reported "R on a merging member breaks the
    // remove and runs /restack anyway"). Refuse on the archive flag
    // (survives the whole teardown window) and on the authoritative
    // on-disk flock (covers a destroy the child already grabbed).
    if (current.archived) {
      toast(`${current.wt.slug} is being cleaned up — not restacking`, theme.warn, 3000);
      return;
    }
    const busy = lockStatus(current.wt.slug);
    if (busy) {
      toast(`${current.wt.slug} is ${lockLabel(busy)} — not restacking`, theme.warn, 3000);
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
   * The chain identity a restack of `branch` occupies in the UI-level
   * busy set: the containing stack's id when the row is a member, else
   * the branch itself (a standalone one-member chain). Matches the
   * `stackId` the automations engine gates on, so a manual `R` and an
   * auto-restack of the same stack exclude each other.
   */
  function restackKeyFor(branch: string): string {
    const row = rows.find((r) => r.wt.branch === branch);
    return row?.stack?.stackId ?? branch;
  }

  /**
   * Conflict-bail handoff: inject the restack skill into the failing
   * worktree's primary harness session (cold-starting it if needed),
   * so `R` completes the same loop `/restack` runs by hand — the
   * engine does the mechanical replay, the LLM takes over exactly at
   * the judgment call the engine refuses to make. Same
   * `injectIntoSession` primitive session-target actions use, minus
   * `launchAction`'s busy guards: the row's cached lock state still
   * reads busy for a beat after the engine released its flocks, and
   * we KNOW the true state here — the bail itself just freed the
   * locks and left the tree clean. Fire-and-forget like every session
   * inject; progress lands in the activity pane. Returns whether the
   * handoff was dispatched (false = no live row for the branch; the
   * caller falls back to the manual-toast wording).
   */
  function handOffConflictToSession(
    failedBranch: string,
    detail: string,
    backupBranch: string | undefined,
  ): boolean {
    const row = rows.find((r) => r.wt.branch === failedBranch);
    if (!row) return false;
    const slug = row.wt.slug;
    const log = createLogger(slug);
    // Don't cold-start a harness session in a worktree that's gone or
    // being torn down. A conflict usually means active work (so this is
    // rare), but the fire-and-forget inject reads a per-render `rows`
    // snapshot — if the worktree was cleaned in the meantime, injecting
    // would spawn a session with cwd inside a deleted directory.
    if (row.archived || lockStatus(slug) || !existsSync(row.wt.path)) {
      log.event.warn(
        `conflict on ${failedBranch}, but its worktree is gone/being cleaned — resolve by hand`,
      );
      return false;
    }
    const harness = getHarness(primaryHarness);
    const skill = `${harness.skillPrefix}restack`;
    const backup = backupBranch
      ? ` The pre-rebase tip is backed up at ${backupBranch}.`
      : "";
    const text = `${skill}\n\nwt's restack engine just bailed on this worktree: ${detail}.${backup} Resolve the conflict and finish the restack.`;
    log.event.info(`conflict — sending ${skill} to ${harness.label} session`);
    void injectIntoSession({
      slug,
      cwd: row.wt.path,
      harnessId: primaryHarness,
      text,
    }).then((res) => {
      if (res.ok) {
        log.event.ok(
          res.coldStarted
            ? `started ${harness.label} session and sent ${skill}`
            : `sent ${skill} to ${harness.label} session`,
        );
      } else {
        log.event.err(`${skill} handoff failed: ${res.reason} — run it by hand`);
        toast(`${skill} handoff failed: ${res.reason}`, theme.err, 5000);
      }
    });
    return true;
  }

  /**
   * The branch-resolved half of `doReplayStack`, shared with the
   * automations engine (`builtin:restack` dispatches here after
   * pre-cleaning the merged members via `doCleanSlugs`). `stackId` is
   * any branch in the target stack (the engine resolves the whole
   * chain from it). "busy" means NOTHING ran — this chain already has a
   * restack in flight (a manual `R` or another auto-restack), or the
   * engine found a member's per-slug lock held (a destroy, another
   * process's restack) — and the automations engine un-consumes the
   * fire on that outcome instead of recording a restack that never
   * happened. Other chains restack concurrently. "clean" / "failed"
   * report the replay itself; a conflict bail reports "failed" AND
   * hands the failing worktree off to the restack skill in its session
   * (see `handOffConflictToSession`), for the manual and automation
   * paths alike.
   */
  async function doRestackStack(
    stackId: string,
  ): Promise<"clean" | "failed" | "busy"> {
    const key = restackKeyFor(stackId);
    if (restackBusyRef.current.has(key)) {
      toast("restack already running for this stack", theme.warn, 2000);
      return "busy";
    }
    restackBusyRef.current.add(key);
    let outcome: "clean" | "failed" | "busy" = "failed";
    appLog.event.info(`restack ${stackId}: fetch + reconcile + replay`);
    try {
      const res = await rebaseStack(stackId, {}, (line) =>
        appLog.event.dim(`restack ${stackId}: ${line}`),
      );
      if (res.ok) {
        outcome = "clean";
        appLog.event.ok(`restacked ${stackId}: ${res.output}`);
        toast(`restacked ${stackId}`, theme.ok, 2500);
      } else if (!res.conflict && res.error === STACK_BUSY) {
        // A member's per-slug lock was held the whole acquire window —
        // nothing ran. Report busy so automations un-consume the fire.
        outcome = "busy";
        appLog.event.warn(`restack ${stackId}: ${res.error}`);
        toast(`restack: ${res.error}`, theme.warn, 4000);
      } else if (res.conflict) {
        const where = res.failedBranch ? ` on ${res.failedBranch}` : "";
        const backup = res.backupBranch ? ` (backup ${res.backupBranch})` : "";
        appLog.event.warn(`restack ${stackId}: conflict${where}${backup}`);
        // Hand the judgment call to the LLM: inject the restack skill
        // into the failing worktree's session. Falls back to the manual
        // hint only when the branch has no live row to inject into.
        const handedOff = res.failedBranch
          ? handOffConflictToSession(res.failedBranch, res.error, res.backupBranch)
          : false;
        toast(
          handedOff
            ? `conflict${where} — handing off to /restack in its session`
            : `conflict${where} — run /restack`,
          theme.warn,
          6000,
        );
      } else {
        appLog.event.err(`restack ${stackId} failed: ${res.error}`);
        toast(`restack failed: ${res.error}`, theme.err, 6000);
      }
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      appLog.event.err(`restack ${stackId} crashed: ${msg}`);
      toast(`restack crashed: ${msg}`, theme.err, 6000);
    } finally {
      restackBusyRef.current.delete(key);
    }
    // PR bases shift when slices move, so refresh the github query (keyed by
    // branch list, not slug) alongside the worktree state.
    void refreshGithub();
    void refreshAll();
    return outcome;
  }

  /**
   * Non-mutating peek at one chain's restack-in-flight state, for the
   * automations engine's dispatch gate: a restack intent for a stack
   * stays queued while a manual `R` (or another auto-restack) runs on
   * THAT stack, BEFORE it pre-cleans anything — cleaning first and then
   * finding the chain busy would strand the stack with its trigger
   * condition already consumed by the clean. Other stacks' restacks
   * don't block it.
   */
  function isRestackBusy(stackId: string): boolean {
    return restackBusyRef.current.has(restackKeyFor(stackId));
  }

  return {
    doRemove,
    doRemoteRemove,
    doClean,
    doCleanSlugs,
    doReplayStack,
    doRestackStack,
    isRestackBusy,
  };
}
