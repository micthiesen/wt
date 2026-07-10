/**
 * Harness-session flows (enter / spawn-named / kill), extracted from
 * `app.tsx`. Same pattern as `flows/destroy.ts`: `makeSessionFlows` is
 * called per render with the current rows + helpers so the returned
 * closures always see fresh state.
 */
import type { CliRenderer } from "@opentui/core";

import {
  addClaudeName,
  nameInUse,
  removeClaudeName,
} from "../../core/harness/claude/names.ts";
import {
  getHarness,
  type Harness,
  type HarnessId,
  type HarnessSession,
} from "../../core/harness/index.ts";
import { createLogger } from "../../core/logger.ts";
import {
  claudeSessionName,
  killHarnessSession,
  listSessions as listTmuxSessions,
} from "../../core/tmux.ts";
import { StatusKind } from "../../core/types.ts";

import { enterHarnessSession } from "../sessions/harness.ts";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";
import type { SessionSlot } from "../sessions/slots.ts";
import { theme } from "../theme.ts";

const appLog = createLogger("[app]");

export function slotSessionResumeTarget(
  harness: Pick<Harness, "singleSlot">,
  slotAlive: boolean,
  sessions: readonly HarnessSession[],
): { resumeSessionId: string | null; freshSlot: boolean } {
  if (!harness.singleSlot || slotAlive) {
    return { resumeSessionId: null, freshSlot: false };
  }
  let latest: HarnessSession | null = null;
  for (const session of sessions) {
    if (
      !latest ||
      (session.lastActiveMs ?? 0) > (latest.lastActiveMs ?? 0)
    ) {
      latest = session;
    }
  }
  return {
    resumeSessionId: latest?.sessionId ?? null,
    freshSlot: latest !== null,
  };
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
    if (row.status.kind === StatusKind.Busy) {
      toast(`${slug} is busy`, theme.warn, 2000);
      return;
    }
    const harness = getHarness(harnessId);
    const sessionLog = createLogger(slug);
    void (async () => {
      sessionLog.event.info(
        `entering ${harness.label} session (F12 to detach)`,
      );
      const result = await enterHarnessSession({
        renderer,
        slug,
        cwd: row.wt.path,
        harnessId,
        managedName: opts.managedName ?? null,
        resumeSessionId: opts.resumeSessionId ?? null,
        freshSlot: opts.freshSlot,
      });
      // Refresh both together so the picker doesn't see a transient
      // state where tmux says "slot dead" but discovery still has the
      // session marked live (or vice versa) and the synthetic-row
      // logic in useHarnessSessions decides incorrectly.
      await Promise.all([refreshTmuxSessions(), refreshHarnessSessions(slug)]);
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
    })();
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
    void (async () => {
      slotLog.event.info(`entering ${harness.label} session (F12 to detach)`);
      let resumeSessionId: string | null = null;
      let freshSlot = false;
      if (harness.singleSlot) {
        const tmuxName = harness.tmuxSessionName(slot.slug, null);
        const liveTmux = await listTmuxSessions().catch(() => null);
        const slotAlive = liveTmux?.all.has(tmuxName) ?? false;
        let sessions: readonly HarnessSession[] = [];
        if (!slotAlive) {
          sessions = await harness
            .discoverSessions({ slug: slot.slug, wtPath: slot.path })
            .catch((err: unknown) => {
              slotLog.event.warn(
                `${harness.label} session discovery failed: ${
                  err instanceof Error ? err.message : String(err)
                }`,
              );
              return [];
            });
        }
        const target = slotSessionResumeTarget(harness, slotAlive, sessions);
        resumeSessionId = target.resumeSessionId;
        freshSlot = target.freshSlot;
      }
      const result = await enterHarnessSession({
        renderer,
        slug: slot.slug,
        cwd: slot.path,
        harnessId: primaryHarness,
        // Surface the slot's label in claude's /resume listing so the
        // conversation is recognizable by name; ignored by codex /
        // opencode (their tmux name is the discriminator).
        claudeDisplayName: slot.label,
        resumeSessionId,
        freshSlot,
      });
      // Refresh tmux sessions so the bottom-bar tail picks up the new
      // session immediately rather than waiting for the next poll
      // tick. `allSettled` (not `all`) so a refresh-rejection — e.g.
      // a torn-down query client during shutdown — doesn't bubble up
      // and swallow the result-feedback below.
      // Harness-session discovery is row-keyed, so there's no slot
      // entry to refresh there; only tmux matters.
      await Promise.allSettled([refreshTmuxSessions()]);
      if (result.kind === "spawn-failed") {
        slotLog.event.err(
          `${harness.label} failed to start: ${result.reason}`,
        );
        toast(`${harness.label} failed: ${result.reason}`, theme.err, 3000);
      } else if (result.kind === "detached") {
        slotLog.event.info(`detached from ${harness.label} session`);
      } else {
        slotLog.event.info(
          `${harness.label} exited (${result.code ?? "?"})`,
        );
        if (result.stderr) slotLog.event.err(result.stderr);
      }
    })();
  }

  /**
   * Spawn-and-attach a brand new named claude session for `slug`.
   * `name` is presumed already validated (caller layer enforces).
   * Persists the name so the session shows up in the picker as a
   * ghost when tmux is dead but the conversation jsonl survives.
   * If `name` already exists in state, this is a resume (no
   * duplicate state mutation).
   *
   * Persist-before-spawn is intentional: a wt crash mid-spawn must
   * leave the name reachable on next start. The trade-off is a
   * spawn-failure window where we'd persist a name for a session
   * that never started; we roll that back below by only persisting
   * fresh names (not pre-existing ones) and removing on spawn-fail.
   */
  function doSpawnNamedClaudeSession(slug: string, name: string): void {
    const row = rows.find((r) => r.wt.slug === slug);
    if (!row) {
      toast(`no row for ${slug}`, theme.warn, 1500);
      return;
    }
    if (row.status.kind === StatusKind.Busy) {
      toast(`${slug} is busy`, theme.warn, 2000);
      return;
    }
    const wasPersisted = nameInUse(slug, name);
    addClaudeName(slug, name);
    void refreshClaudeSummaries(slug);
    const sessionLog = createLogger(slug);
    void (async () => {
      sessionLog.event.info(`entering claude session "${name}" (F12 to detach)`);
      const result = await enterHarnessSession({
        renderer,
        slug,
        cwd: row.wt.path,
        harnessId: "claude",
        managedName: name,
      });
      void refreshTmuxSessions();
      if (result.kind === "spawn-failed") {
        // Roll back the optimistic add IFF we created the entry —
        // if `name` was already in the file (resume case), leave it
        // so the user can retry from the picker.
        if (!wasPersisted) removeClaudeName(slug, name);
        sessionLog.event.err(`claude failed to start: ${result.reason}`);
        toast(`claude failed: ${result.reason}`, theme.err, 3000);
      } else if (result.kind === "detached") {
        sessionLog.event.info(`detached from ${claudeSessionName(slug, name)}`);
      } else {
        sessionLog.event.info(`claude exited (${result.code ?? "?"})`);
        if (result.stderr) sessionLog.event.err(result.stderr);
      }
    })();
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
      void refreshClaudeSummaries(slug);
    }
    void (async () => {
      try {
        // killHarnessSession routes both primary (`name === null`)
        // and named claude sessions through the same call — same
        // implementation as the legacy `killSession` /
        // `killClaudeNamedSession` pair, one source of truth.
        await killHarnessSession(slug, "claude", name);
        appLog.event.warn(
          name === null
            ? `killed primary claude session on ${slug}`
            : `killed claude session "${name}" on ${slug}`,
        );
        void refreshTmuxSessions();
      } catch (err) {
        const msg = err instanceof Error ? err.message : String(err);
        appLog.event.err(`kill claude session failed for ${slug}: ${msg}`);
        // Refetch to reconcile against truth — the optimistic remove
        // is wrong if the kill genuinely failed.
        void refreshTmuxSessions();
      }
    })();
  }

  return {
    doEnterHarnessSession,
    doEnterSlotSession,
    doSpawnNamedClaudeSession,
    doKillClaudeSession,
  };
}
