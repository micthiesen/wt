/**
 * Hub-mode keyboard dispatch. Runs INSTEAD of the classic j/k +
 * session-F-key handling when wt is the hub's task pane: navigation
 * moves the task cursor, F10/F11/F12 retarget the right pane instead
 * of attaching a full-screen client, Enter ensures + shows the
 * harness session, and `z` / `P` drive the manual snooze / pin
 * overlay. Everything not claimed here falls through to
 * `handleNormalKey`, so per-row actions (PR keys, yank, restack,
 * pickers…) behave identically in both modes.
 *
 * Physical keys arrive Alt-stripped: the outer tmux server's `M-<x>`
 * root-table bindings `send-keys` the bare key into this pane, so the
 * handlers below match plain keys exactly like classic mode does.
 */
import type { KeyEvent } from "@opentui/core";

import { createLogger } from "../../core/logger.ts";
import type { TaskBucket } from "../../core/task-state.ts";
import { setTaskPinned, setTaskSnooze } from "../../core/wtstate.ts";
import { isPlainLetter, isShiftedLetter } from "../app-helpers.ts";
import type { makeHubFlows } from "../flows/hub.ts";
import type { TaskItem } from "../hooks/useTaskRows.ts";
import { theme } from "../theme.ts";

const hubLog = createLogger("[hub]");

export type HubKeysCtx = {
  tasks: readonly TaskItem[];
  taskIndex: number;
  /** Select by task key (the same `sel` state flows already target by slug). */
  setSel: (key: string | null) => void;
  toggleStackExpanded: (stackKey: string) => void;
  toggleSlotsExpanded: () => void;
  hubFlows: ReturnType<typeof makeHubFlows>;
  /** F9 — flip pane focus (wt runs select-pane itself; see useHubPaneFocus). */
  toggleFocus: () => void;
  /** Open the selected PR task in the browser (reuses the chord-aware opener). */
  openPrUrl: (url: string, number: number) => void;
  toggleDetails: () => void;
  refreshWtState: () => Promise<void>;
  toast: (message: string, color?: string, ms?: number) => void;
  reportActionError: (label: string, err: unknown) => void;
  /** Fallthrough to the classic normal-mode handler. */
  fallthrough: (k: KeyEvent) => void;
};

/** The selected task's stack key, when it is (or belongs to) a stack. */
function stackKeyOf(task: TaskItem | undefined): string | null {
  if (!task) return null;
  if (task.kind === "stack") return task.key;
  if (task.kind === "wt" && task.row.section && task.row.sectionIsStack) {
    return task.row.section;
  }
  return null;
}

export function handleHubKey(k: KeyEvent, ctx: HubKeysCtx): void {
  const {
    tasks,
    taskIndex,
    setSel,
    toggleStackExpanded,
    toggleSlotsExpanded,
    hubFlows,
    toggleFocus,
    openPrUrl,
    toggleDetails,
    refreshWtState,
    toast,
    reportActionError,
    fallthrough,
  } = ctx;
  const task = tasks[taskIndex];

  // F9 — pane-focus toggle, forwarded from the outer server so wt both
  // performs the select-pane AND stamps its indicator (no event-path
  // guesswork).
  if (k.name === "f9" && !k.shift && !k.ctrl && !k.option && !k.meta) {
    toggleFocus();
    return;
  }

  // F7 — focus the task pane (cmd+h's relay; the outer server rebinds
  // M-h here because the literal `h` was the removed-history toggle).
  if (k.name === "f7" && !k.shift && !k.ctrl && !k.option && !k.meta) {
    hubFlows.focusTaskPane();
    return;
  }

  // Esc with no modal open (modals consume Esc upstream) — bounce
  // focus back to the session pane: the inverse of cmd+h, so hopping
  // into the inbox for one action and back needs no chord at all.
  if (k.name === "escape" && !k.shift && !k.ctrl && !k.option && !k.meta) {
    hubFlows.focusSessionPane();
    return;
  }

  // 1-9 — jump straight to the Nth task (cmd+1..9; the pane renders
  // dim ordinals on the first nine rows). A digit reaching this layer
  // means no picker is open (their quick-pick runs upstream).
  if (
    /^[1-9]$/.test(k.sequence ?? "") &&
    !k.shift && !k.ctrl && !k.option && !k.meta
  ) {
    const n = Number(k.sequence) - 1;
    if (n < tasks.length) setSel(tasks[n]!.key);
    return;
  }

  // --- Task-cursor navigation --------------------------------------
  const move = (delta: number): void => {
    if (tasks.length === 0) return;
    const next = Math.max(0, Math.min(tasks.length - 1, taskIndex + delta));
    setSel(tasks[next]!.key);
  };
  if (isPlainLetter(k, "j") || k.name === "down") {
    move(1);
    return;
  }
  if (isPlainLetter(k, "k") || k.name === "up") {
    move(-1);
    return;
  }
  // name+shift checks, not `k.sequence` — inside the hub's tmux,
  // extended-keys (csi-u) encodes Shift+letter as an escape sequence,
  // so sequence-equality against "G"/"P"/… silently never matches for
  // direct typing (the Alt layer's send-keys still delivers literals).
  if (isPlainLetter(k, "g")) {
    if (tasks.length > 0) setSel(tasks[0]!.key);
    return;
  }
  if (isShiftedLetter(k, "g")) {
    if (tasks.length > 0) setSel(tasks[tasks.length - 1]!.key);
    return;
  }

  // Tab — expand/collapse the selected stack (classic mode folds
  // sections here; the hub's only foldable group is a stack task).
  // The cursor is re-keyed across the toggle: an expanding stack's key
  // vanishes from the task list (members take over, keyed by slug) and
  // a collapsing member's slug vanishes likewise — without the re-key
  // the resolver misses and the cursor snaps to the top.
  if (k.name === "tab" && !k.shift && !k.ctrl && !k.option && !k.meta) {
    if (task?.kind === "slot") {
      // The Sessions group folds like a stack. Keep the cursor on the
      // group across the toggle (collapsing from a sub-entry re-keys
      // onto the surviving main entry).
      toggleSlotsExpanded();
      if (task.collapsedGroup) setSel(task.key);
      else setSel("slot:main");
      return;
    }
    const stackKey = stackKeyOf(task);
    if (!stackKey || !task) return;
    toggleStackExpanded(stackKey);
    setSel(task.kind === "stack" ? task.row.wt.slug : stackKey);
    return;
  }

  // --- Right-pane session targeting --------------------------------
  if (k.name === "return" || k.name === "enter") {
    if (!task) return;
    if (task.kind === "pr") {
      openPrUrl(task.pr.url, task.pr.number);
      return;
    }
    if (task.kind === "slot") {
      hubFlows.showSlotSession(task.slot);
      return;
    }
    hubFlows.showTaskSession(task.row, "harness");
    return;
  }
  // Bare F-keys only: the SHIFTED variants keep their classic meanings
  // (Shift+F12 harness picker, Shift+F10/F11 kill confirms) and must
  // fall through to the normal handler — matching them here would
  // silently remap "kill the shell session" to "show the shell
  // session".
  const plainFn = (name: string): boolean =>
    k.name === name && !k.shift && !k.ctrl && !k.option && !k.meta;
  if (plainFn("f12")) {
    if (task?.kind === "slot") hubFlows.showSlotSession(task.slot);
    else if (task && task.kind !== "pr") hubFlows.showTaskSession(task.row, "harness");
    else toast("no session for this task", theme.fgDim, 1500);
    return;
  }
  if (plainFn("f11")) {
    if (task && task.kind !== "pr" && task.kind !== "slot") hubFlows.showTaskSession(task.row, "diff");
    else toast("no worktree for this task", theme.fgDim, 1500);
    return;
  }
  if (plainFn("f10")) {
    if (task && task.kind !== "pr" && task.kind !== "slot") hubFlows.showTaskSession(task.row, "shell");
    else toast("no worktree for this task", theme.fgDim, 1500);
    return;
  }

  // Bottom-pane FOCUS keys (' pick output, [ ] cycle, " jump to
  // events) have no destination in hub mode — the outputs pane isn't
  // rendered; the real session lives in the right tmux pane. Swallow
  // the whole sibling family with feedback instead of silently
  // mutating output-focus state nothing displays. `f` (tail failing CI
  // logs) is deliberately NOT swallowed: it's an action, not a focus
  // key — it falls through and its lines land in the daily app log.
  if (
    k.sequence === "'" ||
    k.sequence === "[" ||
    k.sequence === "]" ||
    k.sequence === '"'
  ) {
    toast("no output pane in hub — the session pane is live", theme.fgDim, 1800);
    return;
  }

  // The classic `,` / `.` / `/` slot keybindings are retired in hub
  // mode — the Sessions group at the bottom of the inbox IS the slot
  // surface. Swallow them so they can't fall through to the global
  // handler's full-screen attach.
  if (k.sequence === "," || k.sequence === "." || k.sequence === "/") {
    return;
  }

  // Ctrl+D (also cmd+W's relay) on a Sessions-slot entry — the classic
  // close handler targets worktree rows only, so slots get their own
  // graceful-close path here. Worktree tasks fall through to it below.
  if (k.ctrl && k.name === "d" && task?.kind === "slot") {
    hubFlows.closeSlotSession(task.slot);
    return;
  }

  // --- Manual task states ------------------------------------------
  // z — snooze toggle: records the CURRENT bucket; the snooze expires
  // by itself when the derived bucket moves on (level semantics, no
  // timers). Worktree-backed tasks only.
  if (isPlainLetter(k, "z")) {
    if (!task || (task.kind !== "wt" && task.kind !== "stack")) {
      toast("snooze applies to worktree tasks", theme.fgDim, 1500);
      return;
    }
    const slug = task.row.wt.slug;
    try {
      if (task.state.snoozed) {
        setTaskSnooze(slug, null);
        toast("unsnoozed", theme.info, 1200);
      } else {
        setTaskSnooze(slug, task.state.bucket satisfies TaskBucket);
        toast(`snoozed until state changes`, theme.info, 1500);
      }
      void refreshWtState();
    } catch (err) {
      reportActionError("snooze", err);
    }
    return;
  }
  // P — pin toggle.
  if (isShiftedLetter(k, "p")) {
    if (!task || (task.kind !== "wt" && task.kind !== "stack")) {
      toast("pin applies to worktree tasks", theme.fgDim, 1500);
      return;
    }
    const slug = task.row.wt.slug;
    try {
      const next = !task.manual.pinned;
      setTaskPinned(slug, next);
      toast(next ? "pinned" : "unpinned", theme.info, 1200);
      void refreshWtState();
    } catch (err) {
      reportActionError("pin", err);
    }
    return;
  }

  // I — toggle the stacked details card (info). Deliberately far from
  // `d` (destroy): the old Shift+D binding was one slip away from a
  // destructive confirm.
  if (isShiftedLetter(k, "i")) {
    toggleDetails();
    return;
  }

  // q / Ctrl+C — leave the hub (kill the outer layout; sessions
  // survive on the inner server). Intercepted here because a plain
  // `quit()` would just exit this process and the outer pane's
  // died-hook would immediately respawn it.
  if (isPlainLetter(k, "q") || (k.ctrl && k.name === "c")) {
    hubLog.event.dim("leaving hub");
    hubFlows.hubQuit();
    return;
  }

  fallthrough(k);
}
