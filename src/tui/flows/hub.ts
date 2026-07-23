/**
 * Hub-mode session flows — the counterpart of `flows/sessions.ts` for
 * the tmux-hosted hub layout. Instead of suspending the renderer and
 * attaching a full-screen tmux client, hub mode keeps every session on
 * the inner `-L wt` server and just retargets the hub's right pane at
 * it (`switch-client`), so "entering" a session is instant and never
 * takes over the terminal wt lives in.
 *
 * Same per-render factory pattern as the other flows: closures see
 * fresh rows / primary harness on every render.
 */
import { effectiveBaseOrTrunk } from "../../core/git.ts";
import {
  addClaudeName,
  nameInUse,
  removeClaudeName,
} from "../../core/harness/claude/names.ts";
import { getHarness, type HarnessId } from "../../core/harness/index.ts";
import {
  focusRight,
  killHub,
  showHome,
  switchRight,
} from "../../core/hub.ts";
import { createLogger } from "../../core/logger.ts";
import { taskFocusStore } from "../../core/task-focus.ts";
import {
  ensureSessionDetached,
  killHarnessSession,
  sessionName,
} from "../../core/tmux.ts";
import {
  resolveDiffBase,
  sessionLaunchBlockedReason,
} from "../app-helpers.ts";
import type { useHarnessSessions } from "../hooks/useHarnessSessions.ts";
import { isSyntheticLiveSessionId } from "../hooks/useHarnessSessions.ts";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";
import type { SessionSlot } from "../sessions/slots.ts";
import { theme } from "../theme.ts";

const hubLog = createLogger("[hub]");

/**
 * What the hub's right pane is currently showing, as far as this
 * process knows (it performs every switch, so this only drifts if
 * someone drives the inner tmux server by hand). `task` = the selected
 * task's own session — the steady state; `slot` = a special session
 * (`,` / `.` / `/`), which the task pane surfaces prominently so the
 * user isn't confused about what they're typing into; `home` = the
 * dashboard.
 */
export type HubShown =
  | { kind: "task"; slug: string }
  | { kind: "slot"; label: string }
  | { kind: "home" };

export type HubFlowsCtx = {
  rows: readonly WorktreeRow[];
  primaryHarness: HarnessId;
  /** Session discovery for the SELECTED row (drives the F12 target). */
  currentHarnessSessions: ReturnType<typeof useHarnessSessions>;
  toast: (message: string, color?: string, ms?: number) => void;
  reportActionError: (label: string, err: unknown) => void;
  refreshTmuxSessions: () => Promise<void>;
  refreshClaudeSummaries: (slug: string) => Promise<void>;
  /** Records what the right pane now shows (drives the task-pane indicator). */
  setShown: (shown: HubShown) => void;
  onExit: () => void;
};

export function makeHubFlows(ctx: HubFlowsCtx) {
  const {
    rows,
    primaryHarness,
    currentHarnessSessions,
    toast,
    reportActionError,
    refreshTmuxSessions,
    refreshClaudeSummaries,
    setShown,
    onExit,
  } = ctx;

  /**
   * Point the right pane at `name`, record what it shows now, and
   * optionally stamp the task-focus clock (harness views count as
   * "looked at the output"; diff/shell views don't clear the unread
   * bit).
   */
  async function switchTo(
    name: string,
    focusSlug: string | null,
    shown: HubShown,
  ): Promise<void> {
    const ok = await switchRight(name);
    if (!ok) {
      toast(`could not show ${name}`, theme.warn, 2000);
      return;
    }
    setShown(shown);
    if (focusSlug) taskFocusStore.record(focusSlug);
  }

  /**
   * Show a worktree's session in the right pane, spawning it detached
   * first when nothing is live. `harness` resolves the same F12 target
   * the classic mode would attach to (live session of any harness, else
   * resume/spawn the primary).
   */
  function showTaskSession(
    row: WorktreeRow,
    target: "harness" | "diff" | "shell",
  ): void {
    const slug = row.wt.slug;
    const blocked = sessionLaunchBlockedReason(row);
    if (blocked) {
      toast(`${slug} is ${blocked}`, theme.warn, 2000);
      return;
    }
    void (async () => {
      try {
        if (target === "harness") {
          const f12 = currentHarnessSessions.f12Target;
          if (f12?.isLive) {
            await switchTo(f12.tmuxSessionName, slug, { kind: "task", slug });
            await focusRight();
            return;
          }
          // Match classic F12 exactly: resume the target's OWN slot —
          // for claude that can be a NAMED session, not the primary
          // (`extras.managedName` carries it; dropping it would fork
          // the conversation into the primary slot).
          const harnessId = f12?.harnessId ?? primaryHarness;
          const managedName = f12?.extras.managedName ?? null;
          const resume =
            f12 && !isSyntheticLiveSessionId(f12.sessionId)
              ? f12.sessionId
              : null;
          const res = await ensureSessionDetached({
            slug,
            cwd: row.wt.path,
            kind: harnessId,
            managedName,
            resumeSessionId: resume,
          });
          if (!res.ok) {
            toast(`${getHarness(harnessId).label} failed: ${res.reason}`, theme.err, 3000);
            return;
          }
          if (res.created) {
            hubLog.event.info(`started ${getHarness(harnessId).label} session for ${slug}`);
            void refreshTmuxSessions();
          }
          await switchTo(sessionName(slug, harnessId, managedName), slug, {
            kind: "task",
            slug,
          });
          await focusRight();
          return;
        }
        // diff / shell: ensure the kind's single per-slug session, then
        // show it. The diff command may reference {{base}}, so resolve
        // the row's effective base the same way F11 does in classic mode.
        const base =
          target === "diff"
            ? await effectiveBaseOrTrunk(row.wt.path, resolveDiffBase(row))
            : undefined;
        const res = await ensureSessionDetached({
          slug,
          cwd: row.wt.path,
          kind: target,
          base,
        });
        if (!res.ok) {
          toast(`${target} session failed: ${res.reason}`, theme.err, 3000);
          return;
        }
        if (res.created) void refreshTmuxSessions();
        await switchTo(sessionName(slug, target), null, { kind: "task", slug });
        await focusRight();
      } catch (err) {
        reportActionError(`${target} session`, err);
      }
    })();
  }

  /**
   * Show a slot session (`,` / `.` / `/`) in the right pane. The hub
   * analogue of `doEnterSlotSession` — same primary-harness spawn, no
   * renderer handoff.
   */
  function showSlotSession(slot: SessionSlot): void {
    void (async () => {
      try {
        const res = await ensureSessionDetached({
          slug: slot.slug,
          cwd: slot.path,
          kind: primaryHarness,
          managedName: null,
          claudeDisplayName: slot.label,
        });
        if (!res.ok) {
          toast(`${getHarness(primaryHarness).label} failed: ${res.reason}`, theme.err, 3000);
          return;
        }
        if (res.created) void refreshTmuxSessions();
        await switchTo(sessionName(slot.slug, primaryHarness, null), null, {
          kind: "slot",
          label: slot.label,
        });
        await focusRight();
      } catch (err) {
        reportActionError("slot session", err);
      }
    })();
  }

  /**
   * Hub-safe drop-in for `makeSessionFlows.doEnterHarnessSession` —
   * same signature, so the sessions picker (`;`) and harness-select
   * (`Shift+F12`) modals work unchanged in hub mode. Where classic
   * suspends the renderer and attaches full-screen, this ensures the
   * session detached and retargets the right pane; attaching inside
   * the ~44-col task pane is exactly the failure this replaces.
   */
  function enterHarnessSession(
    slug: string,
    harnessId: HarnessId,
    opts: {
      managedName?: string | null;
      resumeSessionId?: string | null;
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
    const managedName = opts.managedName ?? null;
    void (async () => {
      try {
        // Codex / OpenCode share one tmux slot per slug; a "fresh"
        // spawn or a resume-specific-session must clear it first or
        // the ensure below silently reuses whatever's running (same
        // rationale as `freshSlot` on `attachOrCreate`).
        if (opts.freshSlot) {
          await killHarnessSession(slug, harnessId, managedName);
        }
        const res = await ensureSessionDetached({
          slug,
          cwd: row.wt.path,
          kind: harnessId,
          managedName,
          resumeSessionId: opts.resumeSessionId ?? null,
        });
        if (!res.ok) {
          toast(`${getHarness(harnessId).label} failed: ${res.reason}`, theme.err, 3000);
          return;
        }
        if (res.created) void refreshTmuxSessions();
        await switchTo(sessionName(slug, harnessId, managedName), slug, {
          kind: "task",
          slug,
        });
        await focusRight();
      } catch (err) {
        reportActionError("harness session", err);
      }
    })();
  }

  /**
   * Hub-safe drop-in for `doSpawnNamedClaudeSession` (`; c`). Mirrors
   * the classic flow's persist-before-spawn contract (name reachable
   * after a crash mid-spawn; rolled back only when WE created it and
   * the spawn failed) — just detached + retargeted instead of attached.
   */
  function spawnNamedClaudeSession(slug: string, name: string): void {
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
    const wasPersisted = nameInUse(slug, name);
    addClaudeName(slug, name);
    void refreshClaudeSummaries(slug);
    void (async () => {
      try {
        const res = await ensureSessionDetached({
          slug,
          cwd: row.wt.path,
          kind: "claude",
          managedName: name,
        });
        if (!res.ok) {
          if (!wasPersisted) removeClaudeName(slug, name);
          toast(`claude failed: ${res.reason}`, theme.err, 3000);
          return;
        }
        if (res.created) void refreshTmuxSessions();
        await switchTo(sessionName(slug, "claude", name), slug, { kind: "task", slug });
        await focusRight();
      } catch (err) {
        if (!wasPersisted) removeClaudeName(slug, name);
        reportActionError("named claude session", err);
      }
    })();
  }

  /** Point the right pane back at the home dashboard. */
  function showHomePane(): void {
    void showHome()
      .then((ok) => {
        if (ok) setShown({ kind: "home" });
      })
      .catch((err) => reportActionError("hub home", err));
  }

  /**
   * Follow the task cursor: live session → show it (and stamp focus,
   * since it's now on screen); anything else → home. Fire-and-forget;
   * the caller debounces.
   */
  function followSelection(row: WorktreeRow | undefined): void {
    const f12 = currentHarnessSessions.f12Target;
    if (row && f12?.isLive) {
      void switchTo(f12.tmuxSessionName, row.wt.slug, {
        kind: "task",
        slug: row.wt.slug,
      });
    } else {
      void showHome()
        .then((ok) => {
          if (ok) setShown({ kind: "home" });
        })
        .catch(() => {});
    }
  }

  /**
   * Leave the hub: kill the outer layout session (detaching the user's
   * terminal). Inner-server sessions survive by construction. `onExit`
   * runs as a fallback in case the kill raced (e.g. the hub session was
   * already gone) and this process is still alive.
   */
  function hubQuit(): void {
    void killHub()
      .catch((err) => reportActionError("hub quit", err))
      .finally(() => onExit());
  }

  return {
    showTaskSession,
    showSlotSession,
    enterHarnessSession,
    spawnNamedClaudeSession,
    showHomePane,
    followSelection,
    hubQuit,
  };
}
