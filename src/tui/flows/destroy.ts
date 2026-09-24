/**
 * Destroy / clean / restack flows, extracted from `app.tsx`. Pure
 * functions over an explicit context object — `makeDestroyFlows` is
 * called per render inside `App` with the current rows + action
 * helpers, so the returned closures always see fresh state (same
 * semantics as when these lived inline).
 *
 * Each keystroke entrypoint (`doRemove`, `doClean`, `doReplayStack`, …)
 * is a thin `Effect.runPromise` wrapper — kept Promise-returning
 * because the keyboard/modal-key consumers still fire them through
 * `operationErrors(...).promise(...)` + `forkReported`. The real logic
 * lives in the exported Effect it runs (`removeWorktree`, `cleanRows`,
 * …), composed once per flow rather than threaded through async/await.
 */
import { existsSync } from "node:fs";
import { Data, Effect } from "effect";

import { actionRegistry } from "../../core/actions.ts";
import type { RemoteConfig } from "../../core/config.ts";
import { causeMessage, operationErrors } from "../../core/errors.ts";
import type { RemoteWorktreeSummary } from "../../core/remote-worktrees.ts";
import {
  isRemoteWorktreeTarget,
  remoteWorktreeActionKey,
  type WorktreeTarget,
} from "../../core/worktree-target.ts";
import { StatusKind, type PullRequest } from "../../core/types.ts";
import { getHarness } from "../../core/harness/index.ts";
import {
  resolveAgentRoute,
  sendAgentMessageToRoute,
  type RoutedAgentMessageResult,
} from "../../core/harness/agent-routing.ts";
import { spawnBackgroundRemove, type LifecycleError } from "../../core/lifecycle.ts";
import { lockLabel, lockStatus } from "../../core/locks.ts";
import { createLogger } from "../../core/logger.ts";
import { runRemoteWt } from "../../core/remote.ts";
import { removeShellLog } from "../../core/shell-tail.ts";
import { rebaseStack, STACK_BUSY } from "../../core/stack-ops.ts";
import { killAllSessionsFor } from "../../core/tmux.ts";
import {
  recordRemovedWorktrees,
  type RemovedWorktree,
} from "../../core/wtstate.ts";

import {
  destroyHazard,
  destroyHazardLabel,
  isCleanCandidate,
} from "../app-helpers.ts";
import {
  isRemoteCleanCandidate,
  remoteCleanHazardLabel,
} from "../clean-candidate.ts";
import { forkReported } from "../effect-boundary.ts";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";
import { theme } from "../theme.ts";
import { remoteWorktreeLedgerKey } from "../../core/worktree-ref.ts";

const appLog = createLogger("[app]");

const io = operationErrors("destroy flows");

type ConflictHandoffResult = RoutedAgentMessageResult & {
  skill: string;
  harnessLabel: string;
};

/** A remote `wt rm` exiting non-zero — kept tagged (rather than a bare
 *  `Error`) so it doesn't merge into an untyped failure channel. */
class RemoteRemoveExitError extends Data.TaggedError("RemoteRemoveExitError")<{
  readonly code: number;
}> {
  override get message(): string {
    return `remove failed (exit ${this.code})`;
  }
}

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
    ...(row.issueId !== null ? { issueId: row.issueId } : {}),
    ...(row.githubIssue != null ? { githubIssue: row.githubIssue } : {}),
    ...(row.status.kind === StatusKind.Merged ? { gitState: "merged" as const } : {}),
    ...(row.status.kind === StatusKind.Gone ? { gitState: "gone" as const } : {}),
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
  remoteWorktrees: readonly RemoteWorktreeSummary[];
  remotePullRequests: Readonly<Record<string, PullRequest>> | undefined;
  archivedKeys: ReadonlySet<string>;
  /** Currently-selected row (for `doReplayStack`'s stack resolution). */
  current: WorktreeRow | undefined;
  toast: (message: string, color?: string, ms?: number) => void;
  /** Idempotently mark a slug archived (see `useWtActions.archive`). */
  archive: (slug: string) => void;
  /**
   * Move the cursor off rows that are leaving, BEFORE they're archived.
   * A destroy parks the row in the archived block at the bottom of the
   * board for the length of its teardown, and a key-anchored cursor
   * rides it down there. See `cursorSuccessor`.
   */
  advanceCursorPast: (keys: readonly string[]) => void;
  refreshTmuxSessions: () => Promise<void>;
  /**
   * Scoped post-removal refresh (worktree list + wtState). Destroys use
   * this rather than `refreshAll` — see `refreshAfterRemoval`.
   */
  refreshAfterRemoval: () => Promise<void>;
  refreshAll: () => Promise<void>;
  refreshGithub: () => Promise<void>;
  optimisticRemoveRemoteWorktree: (
    remote: RemoteConfig,
    slug: string,
    run: (() => Promise<void>) | Effect.Effect<void, unknown>,
  ) => Promise<void>;
  /**
   * Re-entry guard for `R`: the set of chains (stack id or standalone
   * branch) with a restack in flight. Same-chain re-presses are refused;
   * different chains run concurrently (the engine's per-slug flocks are
   * the real locks; this just avoids spamming them from the UI).
   */
  restackBusyRef: { current: Set<string> };
};

export function makeDestroyFlows(ctx: DestroyFlowsCtx) {
  const {
    rows,
    remoteWorktrees,
    remotePullRequests,
    archivedKeys,
    toast,
    archive,
    advanceCursorPast,
    refreshTmuxSessions,
    refreshAfterRemoval,
    refreshAll,
    refreshGithub,
    optimisticRemoveRemoteWorktree,
    restackBusyRef,
  } = ctx;

  /**
   * Refresh the LIST (not per-slug fields — the checkout is being
   * deleted out from under them) 600ms after a destroy dispatches, so
   * the just-archived row's removal has a beat to land before the
   * board re-reads it. A failure here is maintenance, not a user-facing
   * outcome the destroy itself should answer for — logged and dropped.
   */
  const removalRefresh: Effect.Effect<void> = Effect.sleep("600 millis").pipe(
    Effect.andThen(io.promise("refresh after removal", refreshAfterRemoval)),
    Effect.catch((error) =>
      Effect.sync(() => {
        appLog.warn("post-removal refresh failed", { err: causeMessage(error.cause) });
      }),
    ),
  );

  /**
   * Effect body of `doRemoteRemove`. Never fails — a remote-remove
   * failure is reported (log + toast) and swallowed, same as the
   * original try/catch.
   */
  const remoteRemoveWorktree = Effect.fn("remoteRemoveWorktree")(function* (
    remote: RemoteConfig,
    slug: string,
    opts: { force?: boolean } = {},
  ): Effect.fn.Return<void> {
    const log = createLogger(`[remote:${remote.label}]`);
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
    yield* Effect.forkDetach(actionRegistry.kill(remoteWorktreeActionKey(remote.host, slug)));
    yield* io.promise("optimistic remote remove", () =>
      optimisticRemoveRemoteWorktree(
        remote,
        slug,
        runRemoteWt(remote, args, { onLine: (line) => log.event.dim(line) }).pipe(
          Effect.flatMap((code) =>
            code === 0
              ? Effect.void
              : Effect.fail(new RemoteRemoveExitError({ code })),
          ),
        ),
      ),
    ).pipe(
      Effect.tap(() =>
        Effect.sync(() => {
          log.event.ok(`removed ${slug} from ${remote.label}`);
          toast(`removed ${slug} from ${remote.label}`, theme.ok, 2200);
        }),
      ),
      Effect.catch((error) =>
        Effect.sync(() => {
          const message = causeMessage(error.cause);
          log.event.err(message);
          toast(`remote remove failed: ${message}`, theme.err, 3500);
        }),
      ),
    );
  });

  function doRemoteRemove(
    remote: RemoteConfig,
    slug: string,
    opts: { force?: boolean } = {},
  ): Promise<void> {
    return Effect.runPromise(remoteRemoveWorktree(remote, slug, opts));
  }

  /** Effect body of `doRemove`. May fail with `LifecycleError` from the
   *  background-spawn step, same as the original's uncaught throw. */
  const removeWorktree = Effect.fn("removeWorktree")(function* (
    slug: string,
    opts: { force?: boolean } = {},
  ): Effect.fn.Return<void, LifecycleError> {
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
      const hazard = destroyHazard(row);
      if (hazard) {
        const label = destroyHazardLabel(hazard);
        if (hazard.kind === "unknown") {
          log.event.warn(`refused: ${label}, retry in a moment`);
          toast(`${slug} state still loading, retry in a moment`, theme.warn, 2500);
        } else {
          log.event.err(`refused: ${label}, press d again to force`);
          toast(`${slug} has ${label}`, theme.err, 3000);
        }
        return;
      }
    } else {
      log.event.warn("force destroy: skipping dirty + unpushed guards");
    }
    // The row is leaving this slot: re-aim the cursor at its neighbor
    // before the archive flag moves it to the bottom of the board.
    advanceCursorPast([slug]);
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
    yield* Effect.forkDetach(actionRegistry.kill(slug));
    // Tear down any interactive sessions (claude, diff, shell) BEFORE
    // the worktree removal starts. Their cwds are inside the worktree;
    // letting the remove race against a live tmux child can leave it
    // writing into a half-deleted directory. killAllSessionsFor is
    // idempotent and fast (just SIGHUPs the tmux session daemons), and
    // never fails (best-effort by construction) — awaited so
    // spawnBackgroundRemove only starts once they're gone.
    yield* killAllSessionsFor(slug);
    void refreshTmuxSessions();
    // Drop the shell-tail log now that the session is gone — the
    // startup reap would catch it eventually, but cleaning up at the
    // source keeps the cache dir tidy without waiting for a restart.
    removeShellLog(slug);
    yield* spawnBackgroundRemove(slug, {
      force,
      destroyStage: row.fields.deploy.data ?? false,
      deleteBranch: true,
    });
    log.event.info(`dispatched destroy${force ? " (force)" : ""}`);
    toast(`dispatched destroy of ${slug}`, theme.info);
    // Refresh the LIST, not this slug's fields: the checkout is being
    // deleted out from under them, so re-running ten git probes in a
    // vanishing directory buys errors, not freshness. The busy glyph
    // comes from the lock watcher either way.
    yield* removalRefresh;
  });

  function doRemove(
    slug: string,
    opts: { force?: boolean } = {},
  ): Promise<void> {
    return Effect.runPromise(removeWorktree(slug, opts));
  }

  /** Effect body of `doRemoveWorktree` — one removal entrypoint;
   *  location is resolved only at execution. */
  const removeWorktreeTarget = Effect.fn("removeWorktreeTarget")(function* (
    target: WorktreeTarget,
    opts: { force?: boolean } = {},
  ): Effect.fn.Return<void, LifecycleError> {
    if (isRemoteWorktreeTarget(target)) {
      return yield* remoteRemoveWorktree(target.location.endpoint, target.slug, opts);
    }
    return yield* removeWorktree(target.slug, opts);
  });

  function doRemoveWorktree(
    target: WorktreeTarget,
    opts: { force?: boolean } = {},
  ): Promise<void> {
    return Effect.runPromise(removeWorktreeTarget(target, opts));
  }

  /** Effect body of `doClean`. */
  const cleanAll = Effect.fn("cleanAll")(function* (): Effect.fn.Return<void, LifecycleError> {
    const localCandidates = rows.filter((r) => isCleanCandidate(r));
    const remoteCandidates = remoteWorktrees.filter((entry) =>
      isRemoteCleanCandidate(
        entry,
        archivedKeys.has(remoteWorktreeLedgerKey(entry.hostKey, entry.slug)),
        remotePullRequests?.[entry.branch],
      ),
    );
    if (localCandidates.length + remoteCandidates.length === 0) {
      appLog.event.dim("clean: nothing to clean");
      toast("nothing to clean", theme.fgDim, 1500);
      return;
    }
    const safeRemoteCandidates = remoteCandidates.filter((entry) => {
      const hazard = remoteCleanHazardLabel(entry);
      if (!hazard) return true;
      createLogger(`[remote:${entry.hostLabel}]`).attention.warn(
        `clean: kept ${entry.slug} — ${hazard} (destroy it with d to force)`,
      );
      return false;
    });
    // Keep the independent local/remote removals concurrent; a
    // dispatch-time failure in one interrupts the others, same as the
    // Promise.all-style fan-out this replaces.
    yield* Effect.all(
      [
        cleanRows(localCandidates),
        ...safeRemoteCandidates.map((entry) =>
          remoteRemoveWorktree(entry.remote, entry.slug),
        ),
      ],
      { concurrency: "unbounded", discard: true },
    );
  });

  function doClean(): Promise<void> {
    return Effect.runPromise(cleanAll());
  }

  /**
   * Effect body of `doCleanSlugs` — scoped clean for the automations
   * engine: destroy just the listed slugs, re-filtered through
   * `isCleanCandidate` against CURRENT rows so a fire computed a
   * render ago can't destroy something that un-merged in between.
   * Silently no-ops on an empty survivor set.
   */
  const cleanSlugs = Effect.fn("cleanSlugs")(function* (
    slugs: readonly string[],
  ): Effect.fn.Return<void, LifecycleError> {
    const want = new Set(slugs);
    const candidates = rows.filter(
      (r) => want.has(r.wt.slug) && isCleanCandidate(r),
    );
    if (candidates.length === 0) return;
    yield* cleanRows(candidates);
  });

  function doCleanSlugs(slugs: readonly string[]): Promise<void> {
    return Effect.runPromise(cleanSlugs(slugs));
  }

  /** Effect body of `doCleanRows`, shared by `cleanAll`/`cleanSlugs`. */
  const cleanRows = Effect.fn("cleanRows")(function* (
    input: readonly WorktreeRow[],
  ): Effect.fn.Return<void, LifecycleError> {
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
      // The sweep NEVER forces. `isCleanCandidate` only says the branch
      // landed; the checkout can still hold work nobody committed, and a
      // merged worktree is exactly the kind someone re-opens a session in
      // ("just one more fix") after the PR went green. Unlike `d`, there
      // is no per-row prompt here to fall back on and — for `builtin:clean`
      // — no human in the loop at all, so a hazard means the row survives
      // the sweep and stays on the board. Attention-level because the
      // whole point of pressing `c` is that rows disappear: a silent
      // skip reads as a completed sweep.
      const hazard = destroyHazard(r);
      if (hazard) {
        createLogger(r.wt.slug).attention.warn(
          `clean: kept — ${destroyHazardLabel(hazard)} (destroy it with d to force)`,
        );
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
    //
    // The whole candidate set goes in as one departing group, so the
    // cursor can't land on the next row this sweep is about to destroy.
    advanceCursorPast(candidates.map((r) => r.wt.slug));
    recordRemovedSnapshots(candidates);
    for (const row of candidates) yield* Effect.forkDetach(actionRegistry.kill(row.wt.slug));
    // Best-effort by construction (killAllSessionsFor never fails): one
    // already-dead or inaccessible tmux session must not prevent the
    // remaining candidates from entering the destroy queue.
    yield* Effect.forEach(
      candidates,
      (row) => killAllSessionsFor(row.wt.slug),
      { concurrency: "unbounded", discard: true },
    );
    void refreshTmuxSessions();
    for (const row of candidates) {
      archive(row.wt.slug);
      removeShellLog(row.wt.slug);
      yield* spawnBackgroundRemove(row.wt.slug, {
        force: false,
        destroyStage: row.fields.deploy.data ?? false,
        deleteBranch: true,
      });
      // "via clean sweep", not "verified clean" — the earlier "(clean)"
      // wording read as a per-row cleanliness assertion and sent a real
      // data-loss investigation chasing a wrong-row bug that wasn't there.
      createLogger(row.wt.slug).event.info("dispatched destroy (via clean sweep)");
    }
    // Cleaning a merged stack member reparents its children's fork-base
    // records inside the background remove itself (the branch delete
    // triggers `reparentBaseReferences`, anchors preserved), so no
    // TUI-side bookkeeping is needed here. The actual replay (rebasing
    // commits off the squashed parent) stays an explicit `R`/`/restack`.
    yield* removalRefresh;
  });

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
  const replaySelectedStack = Effect.fn("replaySelectedStack")(function* (): Effect.fn.Return<void> {
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
    yield* restackChain(current.wt.branch);
  });

  function doReplayStack(): Promise<void> {
    return Effect.runPromise(replaySelectedStack());
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
   * Conflict-bail handoff: send the restack skill to the failing
   * worktree's primary harness session (cold-starting it if needed),
   * so `R` completes the same loop `/restack` runs by hand — the
   * engine does the mechanical replay, the LLM takes over exactly at
   * the judgment call the engine refuses to make. Same
   * `sendSessionMessage` primitive session-target actions use, minus
   * `launchAction`'s busy guards: the row's cached lock state still
   * reads busy for a beat after the engine released its flocks, and
   * we KNOW the true state here — the bail itself just freed the
   * locks and left the tree clean. Fire-and-forget like every session
   * message; progress lands in the activity pane. Returns whether the
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
    // rare), but the fire-and-forget send reads a per-render `rows`
    // snapshot — if the worktree was cleaned in the meantime, sending
    // would spawn a session with cwd inside a deleted directory.
    if (row.archived || lockStatus(slug) || !existsSync(row.wt.path)) {
      log.event.warn(
        `conflict on ${failedBranch}, but its worktree is gone/being cleaned — resolve by hand`,
      );
      return false;
    }
    const backup = backupBranch
      ? ` The pre-rebase tip is backed up at ${backupBranch}.`
      : "";
    // `res.ok`/`res.reason` are data on the SUCCESS value (the target's
    // own verdict); a genuine Effect failure here means the send never
    // even attempted delivery (e.g. the per-target lock blew up) — the
    // old `.then` with no `.catch` left that case an unhandled
    // rejection, which `forkReported` now reports instead.
    forkReported(
      resolveAgentRoute(slug).pipe(
        Effect.flatMap((route) => {
          if (!route || route.choice.harnessId === null) {
            return Effect.succeed<ConflictHandoffResult>({
              ok: false as const,
              reason: route
                ? "could not inspect wt's tmux session registry"
                : `unknown agent target: ${slug}`,
              route,
              skill: "$restack",
              harnessLabel: "agent",
            });
          }
          const harness = getHarness(route.choice.harnessId);
          const skill = `${harness.skillPrefix}restack`;
          const text = `${skill}\n\nwt's restack engine just bailed on this worktree: ${detail}.${backup} Resolve the conflict and finish the restack.`;
          log.event.info(`conflict — sending ${skill} to ${harness.label} session`);
          return sendAgentMessageToRoute(route, text).pipe(
            Effect.map((res): ConflictHandoffResult => ({
              ...res,
              skill,
              harnessLabel: harness.label,
            })),
          );
        }),
        Effect.tap((res) =>
          Effect.sync(() => {
            if (res.ok && res.delivered === false) {
              // Delivery is verified against the target's own transcript,
              // for every harness. An unattended handoff that failed
              // verification must say so, because the conflict is still
              // sitting there.
              log.attention.warn(
                `${res.skill} handoff never reached the ${res.harnessLabel} session — run it by hand`,
              );
            } else if (res.ok) {
              // Toast: the handoff lands well after the restack's own
              // toast expired, and cold starts take seconds — worth an
              // async ack. The payload IS a slash command, so
              // `delivered` is null here: it ran, but a command leaves
              // no prompt entry to confirm against, and claiming
              // otherwise would overstate it.
              log.event.ok(
                `${
                  res.coldStarted
                    ? `started ${res.harnessLabel} session and sent ${res.skill}`
                    : `sent ${res.skill} to ${res.harnessLabel} session`
                }${res.delivered === null ? " (a command's arrival can't be confirmed)" : ""}`,
                { toast: true },
              );
            } else {
              log.event.err(`${res.skill} handoff failed: ${res.reason} — run it by hand`);
              toast(`${res.skill} handoff failed: ${res.reason}`, theme.err, 5000);
            }
          }),
        ),
      ),
      (error) => {
        log.event.err(`restack handoff failed: ${error.message} — run it by hand`);
        toast(`restack handoff failed: ${error.message}`, theme.err, 5000);
      },
    );
    return true;
  }

  /**
   * Effect body of `doRestackStack`, shared with the automations engine
   * (`builtin:restack` dispatches here after pre-cleaning the merged
   * members via `doCleanSlugs`). `stackId` is any branch in the target
   * stack (the engine resolves the whole chain from it). "busy" means
   * NOTHING ran — this chain already has a restack in flight (a manual
   * `R` or another auto-restack), or the engine found a member's
   * per-slug lock held (a destroy, another process's restack) — and the
   * automations engine un-consumes the fire on that outcome instead of
   * recording a restack that never happened. Other chains restack
   * concurrently. "clean" / "failed" report the replay itself; a
   * conflict bail reports "failed" AND hands the failing worktree off
   * to the restack skill in its session (see `handOffConflictToSession`),
   * for the manual and automation paths alike. Never fails: a crash out
   * of `rebaseStack` itself is reported the same as a replay failure.
   */
  const restackChain = Effect.fn("restackChain")(function* (
    stackId: string,
  ): Effect.fn.Return<"clean" | "failed" | "busy"> {
    const key = restackKeyFor(stackId);
    if (restackBusyRef.current.has(key)) {
      toast("restack already running for this stack", theme.warn, 2000);
      return "busy";
    }
    restackBusyRef.current.add(key);
    appLog.event.info(`restack ${stackId}: fetch + reconcile + replay`);
    const outcome = yield* rebaseStack(stackId, {}, (line) =>
      appLog.event.dim(`restack ${stackId}: ${line}`),
    ).pipe(
      Effect.map((res): "clean" | "failed" | "busy" => {
        if (res.ok) {
          appLog.event.ok(`restacked ${stackId}: ${res.output}`);
          toast(`restacked ${stackId}`, theme.ok, 2500);
          return "clean";
        }
        if (!res.conflict && res.error === STACK_BUSY) {
          // A member's per-slug lock was held the whole acquire window —
          // nothing ran. Report busy so automations un-consume the fire.
          appLog.event.warn(`restack ${stackId}: ${res.error}`);
          toast(`restack: ${res.error}`, theme.warn, 4000);
          return "busy";
        }
        if (res.conflict) {
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
          return "failed";
        }
        appLog.event.err(`restack ${stackId} failed: ${res.error}`);
        toast(`restack failed: ${res.error}`, theme.err, 6000);
        return "failed";
      }),
      Effect.catch((error) =>
        Effect.sync((): "clean" | "failed" | "busy" => {
          appLog.event.err(`restack ${stackId} crashed: ${error.message}`);
          toast(`restack crashed: ${error.message}`, theme.err, 6000);
          return "failed";
        }),
      ),
      Effect.ensuring(Effect.sync(() => {
        restackBusyRef.current.delete(key);
      })),
    );
    // PR bases shift when slices move, so refresh the github query (keyed by
    // branch list, not slug) alongside the worktree state.
    void refreshGithub();
    void refreshAll();
    return outcome;
  });

  function doRestackStack(
    stackId: string,
  ): Promise<"clean" | "failed" | "busy"> {
    return Effect.runPromise(restackChain(stackId));
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
    doRemoveWorktree,
    doClean,
    doCleanSlugs,
    doReplayStack,
    doRestackStack,
    isRestackBusy,
    removeWorktree,
    remoteRemoveWorktree,
    removeWorktreeTarget,
    cleanAll,
    cleanRows,
    cleanSlugs,
    replaySelectedStack,
    restackChain,
  };
}
