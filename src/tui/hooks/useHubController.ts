/**
 * Hub-mode orchestration, extracted from `app.tsx` (the composition
 * root only routes — this hook owns the hub's stateful glue):
 *
 *  - the `HubShown` record of what the right pane displays, plus the
 *    switch-sequencing refs `makeHubFlows` coordinates through;
 *  - the pane-focus indicator (wraps `useHubPaneFocus`) and the F9
 *    toggle, with wt performing the `select-pane` itself so the
 *    indicator can never drift from an action wt performed;
 *  - the 150ms live-follow of the task cursor;
 *  - the modal focus dance (pull focus for pickers/prompts, restore
 *    the PRE-modal pane on close);
 *  - the shown-session liveness watch: when the session on screen dies
 *    (kill confirm, `;`-picker kill, a crashed pane respawning back to
 *    home), reset to home and re-follow, so a later F-key press
 *    relaunches instead of "toggling focus" onto a dead pane;
 *  - the on-screen-equals-seen focus stamping;
 *  - the startup key mute (see `isMuted`).
 *
 * Classic mode calls this with `enabled: false`; every effect
 * short-circuits and the returned flows are inert-but-typed.
 */
import { useEffect, useMemo, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";

import { focusLeft, focusRight, unzoom, zoomLeft } from "../../core/hub.ts";
import type { HarnessId } from "../../core/harness/index.ts";
import { taskFocusStore } from "../../core/task-focus.ts";
import {
  claudeSessionName,
  listAllSessionsRaw,
  sessionName,
} from "../../core/tmux.ts";
import { tmuxSessionsQuery } from "../../state/queries.ts";
import { makeHubFlows, type HubShown } from "../flows/hub.ts";
import { isRemoteSummary } from "../remote-creation.ts";
import type { ActiveSessionGlyph } from "./useHarnessSessions.ts";
import type { useHarnessSessions } from "./useHarnessSessions.ts";
import { useHubPaneFocus } from "./useHubPaneFocus.ts";
import { freshestTail, type TaskItem } from "./useTaskRows.ts";
import type { WorktreeRow } from "./useWorktreeRows.ts";

export type HubControllerOpts = {
  enabled: boolean;
  tasks: readonly TaskItem[];
  taskIndex: number;
  rows: readonly WorktreeRow[];
  currentHarnessSessions: ReturnType<typeof useHarnessSessions>;
  primaryHarness: HarnessId;
  /** Live Sessions-slot glyphs (presence = live session for that slot). */
  slotGlyphs: ReadonlyMap<string, ActiveSessionGlyph>;
  /** True when the configured `[remote]` host is known-unreachable. */
  remoteUnavailable: boolean;
  /** True while a modal or footer prompt owns typing. */
  inputActive: boolean;
  toast: (message: string, color?: string, ms?: number) => void;
  reportActionError: (label: string, err: unknown) => void;
  refreshTmuxSessions: () => Promise<void>;
  refreshClaudeSummaries: (slug: string) => Promise<void>;
  onExit: () => void;
};

export function useHubController(opts: HubControllerOpts) {
  const {
    enabled,
    tasks,
    taskIndex,
    rows,
    currentHarnessSessions,
    primaryHarness,
    slotGlyphs,
    remoteUnavailable,
    inputActive,
    toast,
    reportActionError,
    refreshTmuxSessions,
    refreshClaudeSummaries,
    onExit,
  } = opts;

  // Keypresses in the first moments of a hub pane are suspect: the
  // startup window is when terminal-query replies (answered through
  // tmux, sometimes split across reads) can masquerade as keys — a
  // leaked "?" used to open the help overlay on launch. The mute is
  // TARGETED rather than total so a fast typist's real j/k/Enter goes
  // through immediately: within the window we only drop keys whose
  // sequence looks like a query-reply fragment (digits, ';', '?',
  // '[', ']', '~', 'u', 'c', 'R' — CSI-u acks, DA replies, cursor
  // reports) or a bare ESC-prefixed chunk. This gate runs ahead of
  // even modal dispatch by necessity: the leak itself can OPEN a
  // modal, so no later layer can be trusted to see clean input.
  const muteUntilRef = useRef(Date.now() + 800);
  function isMuted(k: { sequence?: string }): boolean {
    if (!enabled || Date.now() >= muteUntilRef.current) return false;
    const seq = k.sequence ?? "";
    if (seq.charCodeAt(0) === 0x1b) return true;
    return /^[\d;?\[\]~ucR]$/.test(seq);
  }

  const { focused: paneFocused, setFocused: setPaneFocused } =
    useHubPaneFocus(enabled);
  const paneFocusedRef = useRef(paneFocused);
  paneFocusedRef.current = paneFocused;

  const [hubShown, setHubShown] = useState<HubShown>({ kind: "home" });
  const hubShownRef = useRef(hubShown);
  hubShownRef.current = hubShown;

  // Cross-render coordination for the flows (see flows/hub.ts header).
  const seqRef = useRef(0);
  const pendingTargetRef = useRef<string | null>(null);
  const focusOpRef = useRef<Promise<void>>(Promise.resolve());

  const hubFlows = makeHubFlows({
    rows,
    primaryHarness,
    currentHarnessSessions,
    toast,
    reportActionError,
    refreshTmuxSessions,
    refreshClaudeSummaries,
    remoteUnavailable,
    setShown: setHubShown,
    setPaneFocused,
    getShown: () => hubShownRef.current,
    isPaneFocused: () => paneFocusedRef.current,
    seqRef,
    pendingTargetRef,
    focusOpRef,
    onExit,
  });
  const hubFlowsRef = useRef(hubFlows);
  hubFlowsRef.current = hubFlows;

  const hubTask = enabled ? tasks[taskIndex] : undefined;
  const hubRow =
    hubTask && (hubTask.kind === "wt" || hubTask.kind === "stack")
      ? hubTask.row
      : undefined;
  const hubTaskRef = useRef(hubTask);
  hubTaskRef.current = hubTask;
  const hubRowRef = useRef(hubRow);
  hubRowRef.current = hubRow;
  const slotGlyphsRef = useRef(slotGlyphs);
  slotGlyphsRef.current = slotGlyphs;

  // Live tmux session inventory — drives both the remote-wrapper
  // lookup (follow + glyphs) and the shown-session liveness watch
  // below. Hoisted here because `followNow` needs the wrapper map.
  const tmux = useQuery(tmuxSessionsQuery()).data;
  // Per-remote-slug live wrapper, preferring the harness view over
  // diff/shell when several are open (the follow shows the one you
  // most likely care about; explicit F10/F11 still target the rest).
  // `?? []` tolerates a persisted cache from before the field existed.
  const remoteWrapperBySlug = useMemo(() => {
    const map = new Map<string, { target: "harness" | "diff" | "shell"; name: string }>();
    const rank = { harness: 0, diff: 1, shell: 2 } as const;
    for (const entry of tmux?.remote ?? []) {
      const prev = map.get(entry.slug);
      if (!prev || rank[entry.target] < rank[prev.target]) {
        map.set(entry.slug, { target: entry.target, name: entry.name });
      }
    }
    return map;
  }, [tmux]);
  const remoteWrapperBySlugRef = useRef(remoteWrapperBySlug);
  remoteWrapperBySlugRef.current = remoteWrapperBySlug;

  /** Re-assert the right pane against the current selection (task, slot, or remote). */
  function followNow(): void {
    const t = hubTaskRef.current;
    if (t?.kind === "slot") {
      hubFlowsRef.current.followSlot(
        t.slot,
        slotGlyphsRef.current.has(t.slot.slug),
      );
      return;
    }
    if (t?.kind === "remote") {
      const wrapper = isRemoteSummary(t.entry)
        ? remoteWrapperBySlugRef.current.get(t.entry.slug) ?? null
        : null;
      hubFlowsRef.current.followRemote(t.entry, wrapper);
      return;
    }
    hubFlowsRef.current.followSelection(hubRowRef.current);
  }
  const followNowRef = useRef(followNow);
  followNowRef.current = followNow;

  // Live-follow: the right pane tracks the task cursor — a task with a
  // live session shows it (stamping the focus clock: it's on screen),
  // anything else shows the home dashboard. Debounced so holding j/k
  // doesn't spray switch-clients; keyed on the resolved target so a
  // session going live/dead re-follows without a cursor move. The
  // flows' sequencing makes a stale timer harmless: a debounced follow
  // yields to any explicit switch issued after it was armed.
  const f12 = currentHarnessSessions.f12Target;
  const followKey = enabled
    ? hubTask?.kind === "slot"
      ? `${hubTask.key}:${slotGlyphs.has(hubTask.slot.slug) ? "live" : ""}`
      : hubTask?.kind === "remote"
        ? `${hubTask.key}:${
            isRemoteSummary(hubTask.entry)
              ? remoteWrapperBySlug.get(hubTask.entry.slug)?.name ?? ""
              : ""
          }`
        : `${hubTask?.key ?? ""}:${f12?.isLive ? f12.tmuxSessionName : ""}`
    : "";
  useEffect(() => {
    if (!enabled) return;
    const t = setTimeout(() => {
      followNowRef.current();
    }, 150);
    return () => clearTimeout(t);
  }, [enabled, followKey]);

  // Focus dance: pickers and footer prompts need direct typing, so pull
  // tmux focus onto this pane while one is open and restore the
  // PRE-modal pane on close (answering a picker must not dump you into
  // a session pane you weren't in). The dance also ZOOMS this pane to
  // the full window for the modal's duration — a 20%-inset modal in a
  // ~35-col strip is unusable, so modals render over the area of both
  // panes and the split snaps back on close. Ops ride the same
  // `focusOpRef` queue the flows use, so a picker commit's own
  // focus-session op (enqueued later, when its switch completes) still
  // lands after the restore instead of racing it. Transition-only:
  // acting on the mount value would immediately unfocus the task pane
  // the startup layout deliberately selected.
  // A crash/restart mid-modal would leave the window zoomed with no
  // modal to dismiss it — reassert the split once on startup (cheap
  // state-checked no-op in the normal case).
  useEffect(() => {
    if (enabled) void unzoom();
  }, [enabled]);

  const prevInputActiveRef = useRef<boolean | null>(null);
  const preModalFocusRef = useRef(true);
  useEffect(() => {
    if (!enabled) return;
    const prev = prevInputActiveRef.current;
    prevInputActiveRef.current = inputActive;
    if (prev === null || prev === inputActive) return;
    const queue = (op: () => Promise<void>): void => {
      focusOpRef.current = focusOpRef.current.then(op).catch(() => {});
    };
    if (inputActive) {
      preModalFocusRef.current = paneFocusedRef.current;
      queue(async () => {
        await zoomLeft();
        await focusLeft();
      });
      setPaneFocused(true);
    } else {
      const restore = preModalFocusRef.current;
      queue(async () => {
        await unzoom();
        await (restore ? focusLeft() : focusRight());
      });
      setPaneFocused(restore);
    }
  }, [enabled, inputActive, setPaneFocused]);

  // F9 — flip pane focus. Routed through wt (the outer server forwards
  // it like F10-F12) so the focus indicator is stamped by the very code
  // that moves the focus, never inferred.
  function toggleFocus(): void {
    if (paneFocusedRef.current) hubFlowsRef.current.focusSessionPane();
    else hubFlowsRef.current.focusTaskPane();
  }

  // Refocusing the task pane by hand (F9, mouse) while the right pane
  // shows something other than the selection is "I'm back to the
  // inbox" — re-assert the selection follow. Gated on `inputActive` so
  // the modal focus dance doesn't yank the pane mid-picker, and on the
  // shown state so the common case stays a no-op.
  const inputActiveRef = useRef(inputActive);
  inputActiveRef.current = inputActive;
  useEffect(() => {
    if (!enabled || !paneFocused) return;
    if (inputActiveRef.current) return;
    if (hubShownRef.current.kind === "task") return;
    followNowRef.current();
  }, [enabled, paneFocused]);

  // Shown-session liveness watch: wt performs every switch, but a kill
  // (Shift+F10/F11 confirm, `;`-picker kill) or a crashed right pane
  // respawning to home changes what's on screen without a switch. When
  // the recorded session name drops out of the live tmux set, reset to
  // home and re-follow — otherwise a repeat F-key press would "toggle
  // focus" onto a dead pane instead of relaunching. Remote wrappers
  // are in the set too: a dropped SSH (host asleep, remote session
  // exited) reaps the wrapper and lands here like any other death.
  const liveNames = useMemo(() => {
    if (!enabled || !tmux) return null;
    const names = new Set<string>();
    for (const entry of tmux.claude) {
      names.add(claudeSessionName(entry.slug, entry.name));
    }
    for (const slug of tmux.slugsByHarness.codex) names.add(sessionName(slug, "codex"));
    for (const slug of tmux.slugsByHarness.opencode) names.add(sessionName(slug, "opencode"));
    for (const slug of tmux.diff) names.add(sessionName(slug, "diff"));
    for (const slug of tmux.shell) names.add(sessionName(slug, "shell"));
    for (const entry of tmux.remote ?? []) names.add(entry.name);
    return names;
  }, [enabled, tmux]);
  useEffect(() => {
    if (!enabled || !liveNames) return;
    const shown = hubShownRef.current;
    if (shown.kind === "home") return;
    if (liveNames.has(shown.name)) return;
    // The polled snapshot says the shown session is gone — but a poll
    // whose fetch STARTED before a just-created session was spawned can
    // resolve after the switch landed, and resetting on that stale
    // snapshot would visibly yank a live session off screen (the next
    // poll would show it again 5s later). Death is rarer than
    // switching, so pay one authoritative `list-sessions` re-check
    // before acting; a rerun of this effect cancels a stale check.
    let cancelled = false;
    void listAllSessionsRaw()
      .then((names) => {
        if (cancelled) return;
        const still = hubShownRef.current;
        if (still.kind === "home" || still.name !== shown.name) return;
        if (names.has(shown.name)) return; // stale poll — next refetch catches up
        setHubShown({ kind: "home" });
        followNowRef.current();
      })
      .catch(() => {});
    return () => {
      cancelled = true;
    };
  }, [enabled, liveNames]);

  // On-screen = reviewed: while a task's HARNESS session is displayed,
  // keep its focus stamp fresh whenever new output lands — a turn that
  // finishes in front of you must not file itself under "Review
  // output". Keyed on the shown slug's latest tail entry so the effect
  // re-stamps exactly when output arrives, not per render; the store's
  // own write gate bounds disk traffic.
  const shownHarnessSlug =
    enabled && hubShown.kind === "task" && hubShown.target === "harness"
      ? hubShown.slug
      : null;
  const shownTailMs = useMemo(() => {
    if (!shownHarnessSlug) return null;
    const row = rows.find((r) => r.wt.slug === shownHarnessSlug);
    return freshestTail(row?.fields.claude.data?.sessions)?.lastEntryMs ?? null;
  }, [shownHarnessSlug, rows]);
  useEffect(() => {
    if (!shownHarnessSlug || shownTailMs === null) return;
    const stamped = taskFocusStore.getSnapshot().get(shownHarnessSlug) ?? 0;
    if (stamped >= shownTailMs) return;
    taskFocusStore.record(shownHarnessSlug);
  }, [shownHarnessSlug, shownTailMs]);

  return {
    hubShown,
    paneFocused,
    hubFlows,
    toggleFocus,
    followNow,
    isMuted,
    hubTask,
    hubRow,
    /** Per-remote-slug live SSH wrapper (task-pane glyphs read the keys). */
    remoteWrapperBySlug,
  };
}
