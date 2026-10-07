/**
 * Harness-session flows (enter / spawn-named / kill), extracted from
 * `app.tsx`. Same pattern as `flows/destroy.ts`: `makeSessionFlows` is
 * called per render with the current rows + helpers so the returned
 * closures always see fresh state.
 */
import type { CliRenderer } from "@opentui/core";
import { Effect, Fiber } from "effect";

import { operationErrors } from "../../core/errors.ts";
import {
  addClaudeName,
  nameInUse,
  removeClaudeName,
} from "../../core/harness/claude/names.ts";
import {
  getHarness,
  primarySingleSlotSession,
  type Harness,
  type HarnessId,
  type HarnessSession,
} from "../../core/harness/index.ts";
import { createLogger } from "../../core/logger.ts";
import { canEnterSessionDuringLock } from "../../core/session-readiness.ts";
import {
  claudeSessionName,
  killHarnessSession,
  listSessions as listTmuxSessions,
} from "../../core/tmux.ts";
import { effectiveBaseOrTrunk } from "../../core/git.ts";
import { config } from "../../core/config.ts";
import { remoteWorktreeTarget } from "../../core/worktree-target.ts";
import type { RemoteWorktreeSummary } from "../../core/remote-worktrees.ts";

import { enterHarnessSession } from "../sessions/harness.ts";
import { enterRemoteWorktreeSession } from "../sessions/remote.ts";
import { enterDiffSession } from "../sessions/diff.ts";
import { enterShellSession } from "../sessions/shell.ts";
import type { HarnessRoute } from "../sessions/worktree.ts";
import { forkReported } from "../effect-boundary.ts";
import { resolveDiffBase, sessionLaunchBlockedReason } from "../app-helpers.ts";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";
import type { SessionSlot } from "../sessions/slots.ts";
import { theme } from "../theme.ts";
import type { WorktreeModel } from "../worktree-model.ts";

const appLog = createLogger("[app]");

/**
 * Boundary wrapper for the one remaining genuinely-untyped call site
 * (`harness.discoverSessions`, a `Promise`-returning Harness interface
 * method). Everything else in this module is already a typed Effect
 * (`enterHarnessSession`, `effectiveBaseOrTrunk`, `killHarnessSession`,
 * `listSessions`) and is called directly.
 */
const io = operationErrors("session flows");

export function slotSessionResumeTarget(
  harness: Pick<Harness, "singleSlot">,
  slotAlive: boolean,
  sessions: readonly HarnessSession[],
): { resumeSessionId: string | null; freshSlot: boolean } {
  if (!harness.singleSlot || slotAlive) {
    return { resumeSessionId: null, freshSlot: false };
  }
  const primary = primarySingleSlotSession(sessions);
  return {
    resumeSessionId: primary?.sessionId ?? null,
    freshSlot: primary !== null,
  };
}

/**
 * Persist-before-spawn / conditional-rollback contract for a named
 * claude session, used by `doSpawnNamedClaudeSession` below.
 *
 * Persist-before-spawn is intentional: a wt crash mid-spawn must
 * leave the name reachable on next start. The trade-off is a
 * spawn-failure window where we'd persist a name for a session that
 * never started; this rolls that back — but only when THIS call
 * created the entry (`!wasPersisted`). A resume (name already on
 * disk from an earlier session) is left alone on failure, since a
 * real prior session still owns that entry and removing it would
 * orphan it from the picker.
 *
 * `spawn`'s return type is generic (bounded only by `{ ok: boolean }`)
 * rather than pinned to the caller's full spawn-result shape
 * (`enterHarnessSession`'s `AttachResult`) — this helper only needs
 * the `ok` discriminant to decide whether to roll back. Callers that
 * need the extra detail (e.g. detached-vs-exited messaging) get it
 * back verbatim through the (generic, discriminated) return value /
 * their own closure.
 */
export function withNamedClaudePersistence<T, ERefresh, ESpawn>(
  slug: string,
  name: string,
  // Independently typed: a refresh failure is swallowed below (best
  // effort, never blocks the spawn it races), so only `spawn`'s error
  // type reaches the caller.
  refreshClaudeSummaries: Effect.Effect<void, ERefresh>,
  spawn: Effect.Effect<T, ESpawn>,
  succeeded: (result: T) => boolean,
): Effect.Effect<T, ESpawn> {
  return Effect.gen(function* () {
    const wasPersisted = nameInUse(slug, name);
    addClaudeName(slug, name);
    // Keep the refresh owned by this effect. It may run while the session
    // starts, but it is interrupted and joined before this helper completes.
    const refreshFiber = yield* Effect.forkChild(
      refreshClaudeSummaries.pipe(Effect.catch(() => Effect.void)),
    );
    const result = yield* spawn.pipe(Effect.ensuring(Fiber.interrupt(refreshFiber)));
    // Roll back the optimistic add IFF we created the entry — if `name`
    // was already in the file (resume case), leave it so the user can retry.
    if (!succeeded(result) && !wasPersisted) removeClaudeName(slug, name);
    return result;
  });
}

export type SessionFlowsCtx = {
  rows: readonly WorktreeRow[];
  renderer: CliRenderer;
  primaryHarness: HarnessId;
  toast: (message: string, color?: string, ms?: number) => void;
  refreshTmuxSessions: () => Promise<void>;
  refreshHarnessSessions: (slug: string) => Promise<void>;
  refreshClaudeSummaries: (slug: string) => Promise<void>;
  optimisticRemoveClaude: (slug: string, name: string | null) => void;
  /** True when the selected remote's host is known-unreachable. */
  remoteUnavailable: boolean;
  reportActionError: (label: string, err: unknown) => void;
};

export function makeSessionFlows(ctx: SessionFlowsCtx) {
  const {
    rows,
    renderer,
    primaryHarness,
    toast,
    refreshTmuxSessions,
    refreshHarnessSessions,
    refreshClaudeSummaries,
    optimisticRemoveClaude,
    remoteUnavailable,
    reportActionError,
  } = ctx;

  /**
   * Attach to (or create) a harness session for `slug`. Used for all
   * three harnesses (claude/codex/opencode). For Claude, `managedName`
   * controls primary-vs-named; for Codex/OpenCode `managedName` is
   * ignored and `resumeSessionId` selects which session id to resume
   * (null = spawn fresh).
   */
  function doEnterHarnessSession(
    slug: string,
    harnessId: HarnessId,
    opts: {
      managedName?: string | null;
      resumeSessionId?: string | null;
      /**
       * Codex / OpenCode only: kill the existing tmux slot before
       * attaching so a fresh codex/opencode actually spawns. See the
       * `freshSlot` doc on `enterHarnessSession` / `attachOrCreate`
       * for the rationale.
       */
      freshSlot?: boolean;
    } = {},
  ): void {
    const row = rows.find((r) => r.wt.slug === slug);
    if (!row) {
      toast(`no row for ${slug}`, theme.warn, 1500);
      return;
    }
    const blocked = sessionLaunchBlockedReason(row);
    if (blocked) {
      toast(`${slug} is ${blocked}`, theme.warn, 2000);
      return;
    }
    const harness = getHarness(harnessId);
    const sessionLog = createLogger(slug);
    const label = `${harness.label} session`;
    forkReported(
      Effect.gen(function* () {
        const diffBase = yield* effectiveBaseOrTrunk(row.wt.path, resolveDiffBase(row));
        sessionLog.event.info(
          `entering ${harness.label} session (F12 to detach)`,
        );
        const result = yield* enterHarnessSession({
          renderer,
          slug,
          cwd: row.wt.path,
          harnessId,
          managedName: opts.managedName ?? null,
          resumeSessionId: opts.resumeSessionId ?? null,
          freshSlot: opts.freshSlot,
          diffBase,
        });
        // Report the known outcome before discovery: an unhealthy harness
        // can delay or fail that refresh, hiding the startup error itself.
        if (result.kind === "spawn-failed") {
          sessionLog.event.err(`${harness.label} failed to start: ${result.reason}`);
          toast(`${harness.label} failed: ${result.reason}`, theme.err, 3000);
        } else if (result.kind === "detached") {
          sessionLog.event.info(`detached from ${harness.label} session`);
        } else {
          sessionLog.event.info(
            `${harness.label} exited (${result.code ?? "?"})`,
          );
          if (result.stderr) sessionLog.event.err(result.stderr);
        }
        // Refresh both together so the picker doesn't see a transient
        // state where tmux says "slot dead" but discovery still has the
        // session marked live (or vice versa) and the synthetic-row
        // logic in useHarnessSessions decides incorrectly.
        yield* Effect.all(
          [
            io.promise("refresh tmux sessions", refreshTmuxSessions),
            io.promise("refresh harness sessions", () => refreshHarnessSessions(slug)),
          ],
          { concurrency: 2 },
        );
      }),
      (error) => reportActionError(label, error),
    );
  }

  /**
   * Attach to (or create) the harness session for a non-worktree slot
   * (the `.` / `,` keybinds). Mirrors `doEnterHarnessSession` but
   * skips the row lookup + busy guard — slots aren't worktrees, have
   * no per-slug locking, and are guaranteed to exist (registered at
   * module load). Uses the Shift+TAB-cycled primary harness, so a slot
   * matches a row's F12 default.
   */
  function doEnterSlotSession(slot: SessionSlot): void {
    const harness = getHarness(primaryHarness);
    const slotLog = createLogger(slot.label);
    const label = `${harness.label} slot session`;
    forkReported(
      Effect.gen(function* () {
        slotLog.event.info(`entering ${harness.label} session (F12 to detach)`);
        if (slot.claudeName !== null) addClaudeName(slot.slug, slot.claudeName);
        let resumeSessionId: string | null = null;
        let freshSlot = false;
        if (harness.singleSlot) {
          const tmuxName = harness.tmuxSessionName(slot.slug, null);
          const liveTmux = yield* listTmuxSessions();
          const slotAlive = liveTmux.all.has(tmuxName);
          let sessions: readonly HarnessSession[] = [];
          if (!slotAlive) {
            sessions = yield* io.promise("discover harness sessions", () =>
              harness.discoverSessions({ slug: slot.slug, wtPath: slot.path })
            ).pipe(
              Effect.catch((error) => {
                slotLog.event.warn(
                  `${harness.label} session discovery failed: ${error.message}`,
                );
                return Effect.succeed([]);
              }),
            );
          }
          const target = slotSessionResumeTarget(harness, slotAlive, sessions);
          resumeSessionId = target.resumeSessionId;
          freshSlot = target.freshSlot;
        }
        const result = yield* enterHarnessSession({
          renderer,
          slug: slot.slug,
          cwd: slot.path,
          harnessId: primaryHarness,
          managedName: slot.claudeName,
          switchable: false,
          claudeDisplayName: slot.label,
          resumeSessionId,
          freshSlot,
          diffBase: `origin/${config.branch.base}`,
        });
        if (result.kind === "spawn-failed") {
          slotLog.event.err(`${harness.label} failed to start: ${result.reason}`);
          toast(`${harness.label} failed: ${result.reason}`, theme.err, 3000);
        } else if (result.kind === "detached") {
          slotLog.event.info(`detached from ${harness.label} session`);
        } else {
          slotLog.event.info(`${harness.label} exited (${result.code ?? "?"})`);
          if (result.stderr) slotLog.event.err(result.stderr);
        }
        yield* io.promise("refresh tmux sessions", refreshTmuxSessions).pipe(
          Effect.catch(() => Effect.void),
        );
      }),
      (error) => reportActionError(label, error),
    );
  }

  /**
   * Spawn-and-attach a brand new named claude session for `slug`.
   * `name` is presumed already validated (caller layer enforces).
   * Persists the name so the session shows up in the picker as a
   * ghost when tmux is dead but the conversation jsonl survives.
   * If `name` already exists in state, this is a resume (no
   * duplicate state mutation).
   *
   * The persist-before-spawn / rollback contract lives in
   * `withNamedClaudePersistence` above; this function's `spawn`
   * closure captures the full `enterHarnessSession` result in
   * `attachResult` so the detached-vs-exited-vs-failed messaging
   * below — which the shared helper doesn't know about — still has it
   * to work with.
   */
  function doSpawnNamedClaudeSession(slug: string, name: string): void {
    const row = rows.find((r) => r.wt.slug === slug);
    if (!row) {
      toast(`no row for ${slug}`, theme.warn, 1500);
      return;
    }
    const blocked = sessionLaunchBlockedReason(row);
    if (blocked) {
      toast(`${slug} is ${blocked}`, theme.warn, 2000);
      return;
    }
    const sessionLog = createLogger(slug);
    const label = "named claude session";
    forkReported(
      Effect.gen(function* () {
        const diffBase = yield* effectiveBaseOrTrunk(row.wt.path, resolveDiffBase(row));
        sessionLog.event.info(`entering claude session "${name}" (F12 to detach)`);
        const result = yield* withNamedClaudePersistence(
          slug,
          name,
          io.promise("refresh claude summaries", () => refreshClaudeSummaries(slug)),
          enterHarnessSession({
            renderer,
            slug,
            cwd: row.wt.path,
            harnessId: "claude",
            managedName: name,
            diffBase,
          }),
          (result) => result.kind !== "spawn-failed",
        );
        if (result.kind === "spawn-failed") {
          sessionLog.event.err(`claude failed to start: ${result.reason}`);
          toast(`claude failed: ${result.reason}`, theme.err, 3000);
        } else if (result.kind === "detached") {
          sessionLog.event.info(`detached from ${claudeSessionName(slug, name)}`);
        } else {
          sessionLog.event.info(`claude exited (${result.code ?? "?"})`);
          if (result.stderr) sessionLog.event.err(result.stderr);
        }
        yield* io.promise("refresh tmux sessions", refreshTmuxSessions).pipe(
          Effect.catch((error) => Effect.sync(() => {
            sessionLog.warn("tmux refresh after claude session failed", {
              err: error.message,
            });
          })),
        );
      }),
      (error) => reportActionError(label, error),
    );
  }

  /**
   * Kill a claude session for `slug`. `null` = primary (jsonl is
   * preserved; next F12 attaches via --resume). String = a named
   * session; we also drop it from the persistent name list so the
   * picker stops listing it as a ghost. Idempotent.
   */
  function doKillClaudeSession(slug: string, name: string | null): void {
    // Optimistically drop the entry from `tmuxSessionsQuery` cache
    // BEFORE awaiting the kill so the picker / row badge reflect
    // immediately. Without this, a user reopening `;` in the
    // ~hundreds-of-ms window between dispatch and tmux completion
    // would still see the dying session as live and pressing Enter
    // would `tmux new-session -A` it back to life.
    optimisticRemoveClaude(slug, name);
    if (name !== null) {
      removeClaudeName(slug, name);
      forkReported(
        io.promise("refresh claude summaries", () => refreshClaudeSummaries(slug)),
        (error) => reportActionError("refresh claude summaries", error),
      );
    }
    // killHarnessSession routes both primary (`name === null`) and named
    // claude sessions through the same call — same implementation as the
    // legacy `killSession` / `killClaudeNamedSession` pair, one source
    // of truth. It never fails, so the only failures below are the
    // Promise-boundary refresh calls, each handled inline — nothing
    // escapes to `forkReported`'s report, which is why this stays a
    // bare `runFork`.
    Effect.runFork(
      killHarnessSession(slug, "claude", name).pipe(
        Effect.andThen(
          Effect.sync(() => {
            appLog.event.warn(
              name === null
                ? `killed primary claude session on ${slug}`
                : `killed claude session "${name}" on ${slug}`,
            );
          }),
        ),
        Effect.andThen(io.promise("refresh tmux sessions", refreshTmuxSessions)),
        Effect.catch((error) =>
          Effect.sync(() => {
            appLog.event.err(`kill claude session failed for ${slug}: ${error.message}`);
          }).pipe(
            // Refetch to reconcile against truth — the optimistic remove
            // is wrong if the kill genuinely failed.
            Effect.andThen(io.promise("reconcile tmux sessions", refreshTmuxSessions)),
            Effect.catch(() => Effect.void),
          ),
        ),
      ),
    );
  }

  function doEnterCreatedRemoteHarness(row: RemoteWorktreeSummary, harnessId: HarnessId): void {
    // Successful creation and the refreshed inventory prove the checkout exists.
    forkReported(
      enterRemoteWorktreeSession({
        renderer,
        worktree: remoteWorktreeTarget(row),
        target: "harness",
        harnessId,
      }).pipe(Effect.tap((code) => Effect.sync(() => {
        if (code !== 0) toast(`remote session exited ${code}`, theme.warn, 2500);
      }))),
      (error) => reportActionError("remote session", error),
    );
  }

  /**
   * Enter a shell / diff / harness session on the SELECTED remote
   * worktree over SSH. Mirrors `doEnterHarnessSession`'s guard-then-run
   * shape but for a remote target: refuses an in-flight creation, an
   * unreachable host, or a destructive remote operation before handing the
   * terminal to `enterRemoteWorktreeSession`. Lives here (not app.tsx) so all
   * session-entry logic shares one home.
   */
  function doEnterWorktreeSession(
    worktree: WorktreeModel,
    target: "shell" | "diff" | "harness",
    harnessRoute?: HarnessRoute,
  ): void {
    if (worktree.source.kind === "remote") {
      const remote = worktree.target.location.kind === "remote"
        ? worktree.target.location.endpoint
        : null;
      if (!remote) return;
      if (remoteUnavailable) {
        toast(`${remote.label} is unavailable`, theme.warn, 2200);
        return;
      }
      if (
        worktree.status.kind === "busy" &&
        !canEnterSessionDuringLock(
          { op: worktree.status.op ?? "" },
          worktree.exists,
        )
      ) {
        toast(`${worktree.slug} is ${worktree.status.label}`, theme.warn, 2200);
        return;
      }
      forkReported(
        enterRemoteWorktreeSession({
          renderer,
          worktree: worktree.target,
          target,
          harnessId: primaryHarness,
        }).pipe(
          Effect.tap((code) => Effect.sync(() => {
            if (code !== 0) toast(`remote session exited ${code}`, theme.warn, 2500);
          })),
        ),
        (error) => reportActionError("remote session", error),
      );
      return;
    }

    const row = worktree.source.row;
    const blocked = sessionLaunchBlockedReason(row);
    if (blocked) {
      toast(`${worktree.slug} is ${blocked}`, theme.warn, 2000);
      return;
    }
    if (target === "harness") {
      doEnterHarnessSession(worktree.slug, primaryHarness);
      return;
    }
    const sessionLog = createLogger(worktree.slug);
    const label = `${target} session`;
    forkReported(
      Effect.gen(function* () {
        const base = yield* effectiveBaseOrTrunk(worktree.path, resolveDiffBase(row));
        const result = target === "shell"
          ? yield* Effect.sync(() =>
              sessionLog.event.info("entering shell (F10 to detach)")
            ).pipe(
              Effect.andThen(enterShellSession({
                renderer,
                slug: worktree.slug,
                cwd: worktree.path,
                diffBase: base,
                harness: harnessRoute ?? { harnessId: primaryHarness },
              })),
            )
          : yield* Effect.sync(() =>
              sessionLog.event.info(`opening diff vs ${base} (F11 to detach)`)
            ).pipe(
              Effect.andThen(enterDiffSession({
                renderer,
                slug: worktree.slug,
                cwd: worktree.path,
                base,
                harness: harnessRoute ?? { harnessId: primaryHarness },
              })),
            );
        if (result.kind === "spawn-failed") {
          sessionLog.event.err(`${target} failed to start: ${result.reason}`);
          toast(`${target} failed: ${result.reason}`, theme.err, 3000);
        } else if (result.kind === "detached") {
          sessionLog.event.info(`detached from ${target} (${worktree.slug})`);
        } else {
          sessionLog.event.info(`${target} exited (${result.code ?? "?"})`);
          if (result.stderr) sessionLog.event.err(result.stderr);
        }
        yield* io.promise("refresh tmux sessions", refreshTmuxSessions).pipe(
          Effect.catch((error) => Effect.sync(() => {
            sessionLog.warn("tmux refresh after session failed", {
              target,
              err: error.message,
            });
          })),
        );
      }),
      (error) => reportActionError(label, error),
    );
  }

  return {
    doEnterHarnessSession,
    doEnterSlotSession,
    doSpawnNamedClaudeSession,
    doKillClaudeSession,
    doEnterWorktreeSession,
    doEnterCreatedRemoteHarness,
  };
}
