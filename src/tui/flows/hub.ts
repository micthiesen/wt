/**
 * Hub-mode session flows — the counterpart of `flows/sessions.ts` for
 * the tmux-hosted hub layout. Instead of suspending the renderer and
 * attaching a full-screen tmux client, hub mode keeps every session on
 * the inner `-L wt` server and just retargets the hub's right pane at
 * it (`switch-client`), so "entering" a session is instant and never
 * takes over the terminal wt lives in.
 *
 * Same per-render factory pattern as the other flows: closures see
 * fresh rows / primary harness on every render. All cross-render
 * coordination state (switch sequencing, in-flight target, the focus
 * subprocess queue) lives in refs the caller owns (`useHubController`),
 * because the factory itself is rebuilt every render.
 *
 * ## Switch sequencing
 *
 * Every explicit, user-initiated flow (Enter/F-keys, picker commits)
 * bumps `seqRef` synchronously at dispatch; every awaited step in its
 * async chain re-checks the sequence and aborts when a newer action
 * superseded it. The live-follow flows do NOT bump — they capture the
 * current sequence and abort if anything explicit fires afterward.
 * Net effect: last explicit action wins, a slow spawn can never
 * clobber a faster later switch, and a stale debounced follow can
 * never yank the pane away from an explicit F11/F12 target.
 */
import { effectiveBaseOrTrunk } from "../../core/git.ts";
import { getHarness, type HarnessId } from "../../core/harness/index.ts";
import {
  focusLeft,
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
import { withNamedClaudePersistence } from "./sessions.ts";
import type { SessionSlot } from "../sessions/slots.ts";
import { theme } from "../theme.ts";

const hubLog = createLogger("[hub]");

/**
 * What the hub's right pane is currently showing, as far as this
 * process knows (it performs every switch; the shown-session liveness
 * watch in `useHubController` reconciles the cases it can't see, like
 * a kill or a pane respawn snapping back to home). `task` = a
 * worktree-backed session — the steady state; `slot` = one of the
 * Sessions-group entries (main clone / wt source / dotfiles); `home` =
 * the dashboard. `name` is the inner-server tmux session actually on
 * screen — the liveness watch keys on it.
 */
export type HubShown =
  | { kind: "task"; slug: string; target: "harness" | "diff" | "shell"; name: string }
  | { kind: "slot"; label: string; name: string }
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
  /** Records what the right pane now shows (drives the liveness watch). */
  setShown: (shown: HubShown) => void;
  /**
   * Stamp the task pane's focus indicator. Every flow that moves tmux
   * focus must record it in the same stroke — terminal focus events
   * are an unreliable fallback (mouse only), so an unstamped
   * `focusRight()` leaves the tasks pane styled focused while typing
   * actually goes to the session.
   */
  setPaneFocused: (focused: boolean) => void;
  /** Live reads (refs on the caller side, immune to render identity). */
  getShown: () => HubShown;
  isPaneFocused: () => boolean;
  /** Monotone sequence for last-explicit-action-wins (see header). */
  seqRef: { current: number };
  /**
   * Key of the explicit switch currently in flight (`task:<slug>:<target>`
   * or `slot:<label>`), null when idle. Lets a repeat press during the
   * spawn window read as "toggle focus", and lets the follow flows
   * yield to explicit actions.
   */
  pendingTargetRef: { current: string | null };
  /**
   * Serialization queue for focus subprocesses: two rapid F9s must
   * apply their `select-pane`s in dispatch order or the indicator and
   * reality can end up inverted.
   */
  focusOpRef: { current: Promise<void> };
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
    setPaneFocused,
    getShown,
    isPaneFocused,
    seqRef,
    pendingTargetRef,
    focusOpRef,
    onExit,
  } = ctx;

  /** Enqueue a focus subprocess so rapid toggles apply in order. */
  function queueFocusOp(op: () => Promise<void>): void {
    focusOpRef.current = focusOpRef.current.then(op).catch(() => {});
  }

  /** Hand keyboard focus to the session pane AND stamp the indicator. */
  function focusSessionPane(): void {
    queueFocusOp(async () => {
      await focusRight();
    });
    setPaneFocused(false);
  }

  /** And back: focus the task pane, stamping likewise. */
  function focusTaskPane(): void {
    queueFocusOp(async () => {
      await focusLeft();
    });
    setPaneFocused(true);
  }

  /**
   * Point the right pane at `name`, record what it shows now, and
   * optionally stamp the task-focus clock (harness views count as
   * "looked at the output"; diff/shell views don't clear the unread
   * bit). Returns false when the switch failed or was superseded by a
   * newer explicit action — callers must not move focus on false, or
   * the indicator lies about where typing lands.
   */
  async function switchTo(
    name: string,
    focusSlug: string | null,
    shown: HubShown,
    seq: number,
  ): Promise<boolean> {
    if (seqRef.current !== seq) return false;
    const ok = await switchRight(name);
    if (seqRef.current !== seq) return false;
    if (!ok) {
      toast(`could not show ${name}`, theme.warn, 2000);
      return false;
    }
    setShown(shown);
    if (focusSlug) taskFocusStore.record(focusSlug);
    return true;
  }

  /**
   * Show a worktree's session in the right pane, spawning it detached
   * first when nothing is live. `harness` resolves the same F12 target
   * the classic mode would attach to (live session of any harness, else
   * resume/spawn the primary — named claude slots included).
   */
  function showTaskSession(
    row: WorktreeRow,
    target: "harness" | "diff" | "shell",
  ): void {
    const slug = row.wt.slug;
    const targetKey = `task:${slug}:${target}`;
    // Repeat press = focus toggle: the right pane already shows (or an
    // in-flight switch is already bringing up) exactly this session, so
    // F12/F11/F10 flip which pane the keyboard lands in instead of
    // re-switching — enter with one press, hop back with the same key.
    const shown = getShown();
    const alreadyShown =
      shown.kind === "task" && shown.slug === slug && shown.target === target;
    if (alreadyShown || pendingTargetRef.current === targetKey) {
      if (isPaneFocused()) focusSessionPane();
      else focusTaskPane();
      return;
    }
    const blocked = sessionLaunchBlockedReason(row);
    if (blocked) {
      toast(`${slug} is ${blocked}`, theme.warn, 2000);
      return;
    }
    const seq = ++seqRef.current;
    pendingTargetRef.current = targetKey;
    void (async () => {
      try {
        if (target === "harness") {
          const f12 = currentHarnessSessions.f12Target;
          if (f12?.isLive) {
            const shownNext: HubShown = {
              kind: "task",
              slug,
              target,
              name: f12.tmuxSessionName,
            };
            if (await switchTo(f12.tmuxSessionName, slug, shownNext, seq)) {
              focusSessionPane();
            }
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
          const name = sessionName(slug, harnessId, managedName);
          if (
            await switchTo(name, slug, { kind: "task", slug, target, name }, seq)
          ) {
            focusSessionPane();
          }
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
        const name = sessionName(slug, target);
        if (
          await switchTo(name, null, { kind: "task", slug, target, name }, seq)
        ) {
          focusSessionPane();
        }
      } catch (err) {
        reportActionError(`${target} session`, err);
      } finally {
        if (pendingTargetRef.current === targetKey) {
          pendingTargetRef.current = null;
        }
      }
    })();
  }

  /**
   * Show a Sessions-group slot entry (main clone / wt source /
   * dotfiles) in the right pane — reached by selecting the entry and
   * pressing Enter/F12, never by a dedicated keybinding (the classic
   * `,` / `.` / `/` chords are retired in hub mode).
   */
  function showSlotSession(slot: SessionSlot): void {
    const targetKey = `slot:${slot.label}`;
    const shown = getShown();
    if (
      (shown.kind === "slot" && shown.label === slot.label) ||
      pendingTargetRef.current === targetKey
    ) {
      if (isPaneFocused()) focusSessionPane();
      else focusTaskPane();
      return;
    }
    const seq = ++seqRef.current;
    pendingTargetRef.current = targetKey;
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
        const name = sessionName(slot.slug, primaryHarness, null);
        if (
          await switchTo(name, null, { kind: "slot", label: slot.label, name }, seq)
        ) {
          focusSessionPane();
        }
      } catch (err) {
        reportActionError("slot session", err);
      } finally {
        if (pendingTargetRef.current === targetKey) {
          pendingTargetRef.current = null;
        }
      }
    })();
  }

  /**
   * Hub-safe drop-in for `makeSessionFlows.doEnterHarnessSession` —
   * same signature, so the sessions picker (`;`) and harness-select
   * (`Shift+F12`) modals work unchanged in hub mode. Where classic
   * suspends the renderer and attaches full-screen, this ensures the
   * session detached and retargets the right pane; attaching inside
   * the ~35-col task pane is exactly the failure this replaces.
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
    const seq = ++seqRef.current;
    const targetKey = `task:${slug}:harness`;
    pendingTargetRef.current = targetKey;
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
        const name = sessionName(slug, harnessId, managedName);
        if (
          await switchTo(name, slug, {
            kind: "task",
            slug,
            target: "harness",
            name,
          }, seq)
        ) {
          focusSessionPane();
        }
      } catch (err) {
        reportActionError("harness session", err);
      } finally {
        if (pendingTargetRef.current === targetKey) {
          pendingTargetRef.current = null;
        }
      }
    })();
  }

  /**
   * Hub-safe drop-in for `doSpawnNamedClaudeSession` (`; c`). The
   * persist-before-spawn/rollback contract is shared with the classic
   * flow via `withNamedClaudePersistence` — just detached + retargeted
   * instead of attached.
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
    const seq = ++seqRef.current;
    const targetKey = `task:${slug}:harness`;
    pendingTargetRef.current = targetKey;
    void (async () => {
      try {
        const out = await withNamedClaudePersistence(
          slug,
          name,
          refreshClaudeSummaries,
          async () =>
            await ensureSessionDetached({
              slug,
              cwd: row.wt.path,
              kind: "claude",
              managedName: name,
            }),
        );
        if (!out.ok) {
          toast(`claude failed: ${out.reason ?? "spawn failed"}`, theme.err, 3000);
          return;
        }
        void refreshTmuxSessions();
        const tmuxName = sessionName(slug, "claude", name);
        if (
          await switchTo(tmuxName, slug, {
            kind: "task",
            slug,
            target: "harness",
            name: tmuxName,
          }, seq)
        ) {
          focusSessionPane();
        }
      } catch (err) {
        reportActionError("named claude session", err);
      } finally {
        if (pendingTargetRef.current === targetKey) {
          pendingTargetRef.current = null;
        }
      }
    })();
  }

  /**
   * Live-follow for a selected Sessions-slot task: show its live
   * session (never stealing focus), else the home dashboard. The slot
   * counterpart of `followSelection`. Yields to any in-flight or later
   * explicit action (captures the sequence without bumping it).
   */
  function followSlot(slot: SessionSlot, isLive: boolean): void {
    if (pendingTargetRef.current !== null) return;
    const seq = seqRef.current;
    if (isLive) {
      const name = sessionName(slot.slug, primaryHarness, null);
      void switchTo(name, null, { kind: "slot", label: slot.label, name }, seq);
    } else {
      void showHome()
        .then((ok) => {
          if (ok && seqRef.current === seq) setShown({ kind: "home" });
        })
        .catch(() => {});
    }
  }

  /** Point the right pane back at the home dashboard. */
  function showHomePane(): void {
    const seq = seqRef.current;
    void showHome()
      .then((ok) => {
        if (ok && seqRef.current === seq) setShown({ kind: "home" });
      })
      .catch((err) => reportActionError("hub home", err));
  }

  /**
   * Follow the task cursor: live session → show it (and stamp focus,
   * since it's now on screen); anything else → home. Fire-and-forget;
   * the caller debounces. Yields to explicit actions (see header).
   */
  function followSelection(row: WorktreeRow | undefined): void {
    if (pendingTargetRef.current !== null) return;
    const seq = seqRef.current;
    const f12 = currentHarnessSessions.f12Target;
    if (row && f12?.isLive) {
      void switchTo(f12.tmuxSessionName, row.wt.slug, {
        kind: "task",
        slug: row.wt.slug,
        target: "harness",
        name: f12.tmuxSessionName,
      }, seq);
    } else {
      void showHome()
        .then((ok) => {
          if (ok && seqRef.current === seq) setShown({ kind: "home" });
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
    followSlot,
    enterHarnessSession,
    spawnNamedClaudeSession,
    showHomePane,
    followSelection,
    focusSessionPane,
    focusTaskPane,
    hubQuit,
  };
}
