/**
 * Normal-mode keyboard dispatch — every key that acts on the live list
 * view: bottom-pane focus chords, section moves, list navigation,
 * per-PR keys, session F-keys, and the per-row action keys. Extracted
 * VERBATIM from `app.tsx`; the order of the checks below is
 * load-bearing (see the ordering notes inline) — do not reorder.
 *
 * Case-sensitivity note: the raw-stdin keypress parser lowercases
 * `name` for A–Z and sets `shift: true`, so case pairs on the same
 * physical key (`g`/`G`, `r`/`R`, `n`/`N`, `e`/`E`, `o`/`O`) check
 * `k.sequence`, not the `isPlainLetter`/`isShiftedLetter` helpers.
 * Preserve each binding's existing style when editing.
 */
import type { KeyEvent, ScrollBoxRenderable } from "@opentui/core";
import type { RefObject } from "react";

import { actionRegistry } from "../../core/actions.ts";
import { config, type PullRequestTarget } from "../../core/config.ts";
import { effectiveBaseOrTrunk } from "../../core/git.ts";
import { getHarness, HARNESSES, type HarnessId } from "../../core/harness/index.ts";
import { linearUrlForSlug } from "../../core/linear.ts";
import { createLogger } from "../../core/logger.ts";
import { eventsOutputId, indexOfOutput } from "../../core/outputs.ts";
import { stageUrl } from "../../core/stage.ts";
import { closeHarnessSessionGracefully } from "../../core/tmux.ts";
import { StatusKind } from "../../core/types.ts";
import { stackIdFromSectionKey } from "../../core/wtstate.ts";
import { isPlainLetter, isShiftedLetter, resolveDiffBase } from "../app-helpers.ts";
import { enterDiffSession } from "../diff-session.ts";
import { enterShellSession } from "../shell-session.ts";
import type { makeBaseFlows } from "../flows/base.ts";
import type { makeDestroyFlows } from "../flows/destroy.ts";
import type { makeGithubPrFlows } from "../flows/github-pr.ts";
import { REVIEW_SECTION } from "../flows/new-worktree.ts";
import type { makeSectionFlows } from "../flows/sections.ts";
import type { makeSessionFlows } from "../flows/sessions.ts";
import { openInZed, openUrlHidingAlacritty } from "../helpers.ts";
import {
  isSyntheticLiveSessionId,
  type useHarnessSessions,
} from "../hooks/useHarnessSessions.ts";
import type { useOutputFocus } from "../hooks/useOutputFocus.ts";
import { GROUP_INBOX } from "../hooks/useWorktreeRows.ts";
import { visualKey, type useVisualItems } from "../hooks/useVisualItems.ts";
import type { Modal } from "../modal.ts";
import type { FooterMode } from "../panels/footer.tsx";
import type { ListScrollHandle } from "../panels/list.tsx";
import { theme } from "../theme.ts";

const appLog = createLogger("[app]");
const newLog = createLogger("[new]");

type VisualItems = ReturnType<typeof useVisualItems>;
type OutputFocus = ReturnType<typeof useOutputFocus>;

export type NormalKeysCtx = {
  // Bottom-pane focus
  focusedOutputId: OutputFocus["focusedOutputId"];
  setFocus: OutputFocus["setFocus"];
  visibleOutputs: OutputFocus["visibleOutputs"];
  displayedOutput: OutputFocus["displayedOutput"];
  // Cursor / selection
  current: VisualItems["current"];
  currentItem: VisualItems["currentItem"];
  selectedPr: VisualItems["selectedPr"];
  selectedSection: VisualItems["selectedSection"];
  visualItems: VisualItems["visualItems"];
  cursorIndex: VisualItems["cursorIndex"];
  currentSlug: string | undefined;
  setSel: (key: string | null) => void;
  // Chrome state
  setModal: (m: Modal | null) => void;
  setFooter: (f: FooterMode) => void;
  detailsScrollRef: RefObject<ScrollBoxRenderable | null>;
  listScrollHandleRef: RefObject<ListScrollHandle | null>;
  // PR-target chord
  consumePrTargetChord: (k: KeyEvent) => boolean;
  rememberPrTargetChord: (target: PullRequestTarget) => boolean;
  openPrUrl: (
    url: string,
    number: number,
    target: PullRequestTarget | null,
    logName: string,
  ) => void;
  // Sessions
  currentHarnessSessions: ReturnType<typeof useHarnessSessions>;
  primaryHarness: HarnessId;
  activeShellSessions: ReadonlySet<string>;
  activeDiffSessions: ReadonlySet<string>;
  renderer: Parameters<typeof enterShellSession>[0]["renderer"];
  doEnterHarnessSession: ReturnType<typeof makeSessionFlows>["doEnterHarnessSession"];
  // Flows
  handleGlobalKey: (k: KeyEvent) => boolean;
  doShiftMove: ReturnType<typeof makeSectionFlows>["doShiftMove"];
  openSectionPicker: ReturnType<typeof makeSectionFlows>["openSectionPicker"];
  openSectionRename: ReturnType<typeof makeSectionFlows>["openSectionRename"];
  openBasePicker: ReturnType<typeof makeBaseFlows>["openBasePicker"];
  openActionPicker: (slug: string) => void;
  openReviewerPicker: (slug: string) => Promise<void>;
  doReplayStack: ReturnType<typeof makeDestroyFlows>["doReplayStack"];
  doTailFailedChecks: ReturnType<typeof makeGithubPrFlows>["doTailFailedChecks"];
  // Automations
  automations: { configured: boolean };
  toggleAutomationsPaused: (slug: string) => Promise<boolean>;
  toggleStackAutomationsPaused: (stackId: string) => Promise<boolean>;
  // Actions on the row
  toggleArchived: (slug: string) => Promise<{ archived: boolean }>;
  toggleSectionFold: (key: string) => Promise<boolean>;
  refreshAiSummary: (slug: string) => Promise<boolean>;
  refreshTmuxSessions: () => Promise<void>;
  // Feedback
  toast: (message: string, color?: string, ms?: number) => void;
  reportActionError: (label: string, err: unknown) => void;
};

export function handleNormalKey(k: KeyEvent, ctx: NormalKeysCtx): void {
  const {
    focusedOutputId,
    setFocus,
    visibleOutputs,
    displayedOutput,
    current,
    currentItem,
    selectedPr,
    selectedSection,
    visualItems,
    cursorIndex,
    currentSlug,
    setSel,
    setModal,
    setFooter,
    detailsScrollRef,
    listScrollHandleRef,
    consumePrTargetChord,
    rememberPrTargetChord,
    openPrUrl,
    currentHarnessSessions,
    primaryHarness,
    activeShellSessions,
    activeDiffSessions,
    renderer,
    doEnterHarnessSession,
    handleGlobalKey,
    doShiftMove,
    openSectionPicker,
    openSectionRename,
    openBasePicker,
    openActionPicker,
    openReviewerPicker,
    doReplayStack,
    doTailFailedChecks,
    automations,
    toggleAutomationsPaused,
    toggleStackAutomationsPaused,
    toggleArchived,
    toggleSectionFold,
    refreshAiSummary,
    refreshTmuxSessions,
    toast,
    reportActionError,
  } = ctx;
    // Escape clears this worktree's explicit focus so the bottom
    // pane returns to follow-row auto-rules.
    if (k.name === "escape" && focusedOutputId) {
      setFocus(currentSlug ?? null, { focused: null });
      return;
    }
    if (consumePrTargetChord(k)) return;
    // `'` opens the Outputs picker — vim-`:ls` flavor for the bottom
    // pane. Cursor lands on the displayed output within this
    // worktree's filtered list. Outputs is the rarer chord; sessions
    // wins `;`.
    if (k.sequence === "'") {
      const idx = Math.max(
        0,
        indexOfOutput(visibleOutputs, displayedOutput.id),
      );
      setModal({ kind: "outputsPicker", index: idx });
      return;
    }
    // `;` opens the claude sessions picker for the current row.
    // Lists live primary + named sessions plus a "+ new" affordance
    // so the user can spawn additional named sessions in one place.
    // Refuses on rows with no current row (no slug to scope to).
    //
    // Initial cursor: if the bottom pane is currently displaying a
    // claude session for this slug, land on its row so the picker
    // mirrors what the user is already looking at. Otherwise default
    // to primary (entries[0]). Mirrors the outputs picker's "open
    // on the displayed item" pattern.
    if (k.sequence === ";") {
      if (!current) {
        toast("select a worktree first", theme.warn, 1500);
        return;
      }
      if (current.status.kind === StatusKind.Busy) {
        toast(`${current.wt.slug} is busy`, theme.warn, 2000);
        return;
      }
      const slug = current.wt.slug;
      // Initial cursor: if the bottom pane is currently displaying a
      // claude session for this slug, land on its row so the picker
      // mirrors what the user is already looking at. Otherwise default
      // to row 0 (most-recent live session, or "+ new claude" if no
      // sessions exist). Sessions come from `currentHarnessSessions`,
      // sorted by the picker memo on first render.
      let initialIdx = 0;
      if (
        displayedOutput.kind === "session" &&
        displayedOutput.sessionKind === "claude" &&
        displayedOutput.slug === slug
      ) {
        // `sessions` is already in the picker's display order (live
        // first, then recency) from the hook, so the index matches what
        // the picker renders. Falls back to 0 when the displayed session
        // is no longer in the list.
        const matchIdx = currentHarnessSessions.sessions.findIndex(
          (e) =>
            e.harnessId === "claude" &&
            e.extras.managedName === displayedOutput.sessionName,
        );
        if (matchIdx >= 0) initialIdx = matchIdx;
      }
      setModal({ kind: "claudeSessionsPicker", slug, index: initialIdx });
      return;
    }
    // `[` / `]` — cycle prev/next through THIS worktree's visible
    // outputs. Wraps at both ends.
    if (k.sequence === "[" || k.sequence === "]") {
      if (visibleOutputs.length === 0) return;
      const cur = Math.max(
        0,
        indexOfOutput(visibleOutputs, displayedOutput.id),
      );
      const step = k.sequence === "]" ? 1 : -1;
      const next =
        (cur + step + visibleOutputs.length) % visibleOutputs.length;
      const target = visibleOutputs[next];
      if (target) setFocus(currentSlug ?? null, { focused: target.id });
      return;
    }
    // `"` jumps to events for this worktree's bucket — the global
    // pane when you want to step out of whatever per-row context the
    // auto-rules surfaced.
    if (k.sequence === '"') {
      setFocus(currentSlug ?? null, { focused: eventsOutputId() });
      return;
    }
    // Unified Shift+J/K — moves the smallest movable thing under the
    // cursor one display position: a row within/across manual sections
    // and the inbox (chord-holding J walks it through the whole list,
    // hopping over stack sections, never crossing into archived), the
    // WHOLE stack when the cursor is on a stack row, and the whole
    // group when it's on a folded section header.
    if (isShiftedLetter(k, "j")) {
      doShiftMove(1);
      return;
    }
    if (isShiftedLetter(k, "k")) {
      doShiftMove(-1);
      return;
    }
    // Shift+L renames the current row's section.
    if (isShiftedLetter(k, "l")) {
      openSectionRename();
      return;
    }
    // App-level keys shared with the removed-worktrees view (help,
    // quit, refresh, ^R, n, c, Shift+A, Shift+Tab, slot sessions, zed).
    if (handleGlobalKey(k)) return;
    // Ctrl+A — toggle automations for the thing under the cursor
    // (persisted in wtstate, survives restarts). A stack member or a
    // folded stack header pauses/resumes the WHOLE stack as one, keyed
    // by stackId so slices added or re-split later stay covered; a
    // non-stack row toggles just itself. The escape hatch when a
    // branch (or stack) is under manual surgery.
    if (k.ctrl && k.name === "a" && !k.shift && !k.option && !k.meta) {
      if (!automations.configured) {
        toast("no [[automations]] configured", theme.fgDim, 2000);
        return;
      }
      const stackId = selectedSection?.isStack
        ? stackIdFromSectionKey(selectedSection.sectionKey)
        : current?.stack?.stackId ?? null;
      if (!stackId && !current) {
        toast("select a worktree first", theme.warn, 1500);
        return;
      }
      void (async () => {
        try {
          if (stackId) {
            const nowPaused = await toggleStackAutomationsPaused(stackId);
            appLog.event.info(
              nowPaused
                ? `automations paused for stack ${stackId}`
                : `automations resumed for stack ${stackId}`,
            );
            toast(
              nowPaused
                ? `automations paused for stack ${stackId}`
                : `automations resumed for stack ${stackId}`,
              nowPaused ? theme.warn : theme.ok,
              2000,
            );
            return;
          }
          const slug = current!.wt.slug;
          const nowPaused = await toggleAutomationsPaused(slug);
          createLogger(slug).event.info(
            nowPaused ? "automations paused for this worktree" : "automations resumed for this worktree",
          );
          toast(
            nowPaused ? `automations paused for ${slug}` : `automations resumed for ${slug}`,
            nowPaused ? theme.warn : theme.ok,
            2000,
          );
        } catch (err) {
          reportActionError("automations toggle", err);
        }
      })();
      return;
    }
    // Ctrl+J / Ctrl+K page the details pane (worktree or review request)
    // by ~85% of a viewport — for panes too tall to fit, which otherwise
    // clip. No-op when the content fits. List navigation stays on bare
    // j/k; these require Ctrl and only move the right pane. Ctrl+J
    // arrives as "linefeed" in legacy terminals (it's the LF byte,
    // special-cased ahead of ctrl-letter mapping) and as ctrl+"j" under
    // the kitty keyboard protocol — accept both.
    if (k.name === "linefeed" || (k.ctrl && k.name === "j")) {
      detailsScrollRef.current?.scrollBy(0.85, "viewport");
      return;
    }
    if (k.ctrl && k.name === "k") {
      detailsScrollRef.current?.scrollBy(-0.85, "viewport");
      return;
    }
    if (k.name === "j" || k.name === "down") {
      if (visualItems.length === 0) return;
      // Already on the last item — there's nowhere to move the cursor, so
      // scroll the pane to the very bottom instead, revealing any trailing
      // blank space / the review + archived headers below it.
      if (cursorIndex >= 0 && cursorIndex >= visualItems.length - 1) {
        listScrollHandleRef.current?.toEdge("bottom");
        return;
      }
      const nextIdx = Math.min(cursorIndex + 1, visualItems.length - 1);
      const next = visualItems[nextIdx];
      setSel(next ? visualKey(next) : null);
      return;
    }
    if (k.name === "k" || k.name === "up") {
      if (visualItems.length === 0) return;
      // Already on the first item — scroll the pane to the very top.
      if (cursorIndex === 0) {
        listScrollHandleRef.current?.toEdge("top");
        return;
      }
      const nextIdx = Math.max(0, cursorIndex - 1);
      const next = visualItems[nextIdx];
      setSel(next ? visualKey(next) : null);
      return;
    }
    // The raw-stdin keypress parser lowercases `name` for A–Z and sets
    // `shift: true`, so case-sensitive bindings (`g`/`G`, `r`/`R`) have
    // to disambiguate on `sequence` rather than `name`.
    if (k.sequence === "g") {
      rememberPrTargetChord("github");
      const first = visualItems[0];
      setSel(first ? visualKey(first) : null);
      return;
    }
    if (k.sequence === "G") {
      const last = visualItems[visualItems.length - 1];
      setSel(last ? visualKey(last) : null);
      return;
    }
    // `R` — restack the stack the selected worktree belongs to (whole stack,
    // algorithmic; escalates to /restack only on a conflict bail).
    if (k.sequence === "R") {
      void doReplayStack();
      return;
    }
    if (k.sequence === "N") {
      if (!current) {
        toast("select a worktree first", theme.warn, 2000);
        return;
      }
      if (!current.wt.branch) {
        toast("no branch on selected row", theme.warn, 2000);
        return;
      }
      newLog.event.info(`using ${current.wt.branch} as base`);
      setFooter({
        kind: "input",
        prompt: "new:",
        value: "",
        purpose: "new",
        base: current.wt.branch,
      });
      return;
    }
    // `!` — open the action picker for the selected worktree, OR
    // open the kill-confirm if an action is currently running there.
    // Same key both ways so muscle memory stays consistent regardless
    // of state.
    if (k.sequence === "!") {
      if (!current) {
        toast("select a worktree first", theme.warn, 1500);
        return;
      }
      const slug = current.wt.slug;
      const run = actionRegistry.get(slug);
      if (run?.status === "running") {
        setModal({ kind: "killActionConfirm", slug, actionName: run.actionName });
      } else {
        openActionPicker(slug);
      }
      return;
    }
    // F10 — toggle into the selected worktree's plain shell session.
    // Persistent like F12: detach with F10, reattach to find
    // scrollback, env, and any background processes still alive.
    // `exit` / Ctrl+D ends the session.
    if (
      k.name === "f10" &&
      !k.shift &&
      !k.ctrl &&
      !k.option &&
      !k.super &&
      !k.hyper &&
      !k.meta
    ) {
      if (!current) {
        toast("select a worktree first", theme.warn, 1500);
        return;
      }
      const slug = current.wt.slug;
      if (current.status.kind === StatusKind.Busy) {
        toast(`${slug} is busy`, theme.warn, 2000);
        return;
      }
      const cwd = current.wt.path;
      const shellLog = createLogger(slug);
      void (async () => {
        shellLog.event.info("entering shell (F10 to detach)");
        const result = await enterShellSession({ renderer, slug, cwd });
        // Flip the indicator + spin up the shell-tail tailer
        // immediately rather than waiting for the tmux-sessions
        // poll. Without this, lines written in the first seconds
        // arrive only via seed-on-late-ensure, not as live deltas.
        void refreshTmuxSessions();
        if (result.kind === "spawn-failed") {
          shellLog.event.err(`shell failed to start: ${result.reason}`);
          toast(`shell failed: ${result.reason}`, theme.err, 3000);
        } else if (result.kind === "detached") {
          shellLog.event.info(`detached from shell (${slug})`);
        } else {
          shellLog.event.info(`shell exited (${result.code ?? "?"})`);
          if (result.stderr) shellLog.event.err(result.stderr);
        }
      })();
      return;
    }
    // F11 — toggle into the selected worktree's diff TUI
    // (`[diff].command`, default `gitu`). tmux's `new-session -A`
    // makes this idempotent (creates or attaches), and the
    // wt-private tmux config binds F11 to detach-client → the same
    // physical key flips between contexts. Sessions persist (named
    // `<slug>-diff`) so detach-then-reattach keeps gitu's scroll +
    // expansion state. Refuse on busy worktrees so we don't race a
    // destroy.
    if (
      k.name === "f11" &&
      !k.shift &&
      !k.ctrl &&
      !k.option &&
      !k.super &&
      !k.hyper &&
      !k.meta
    ) {
      if (!current) {
        toast("select a worktree first", theme.warn, 1500);
        return;
      }
      const slug = current.wt.slug;
      if (current.status.kind === StatusKind.Busy) {
        toast(`${slug} is busy`, theme.warn, 2000);
        return;
      }
      const cwd = current.wt.path;
      const rawBase = resolveDiffBase(current);
      const diffLog = createLogger(slug);
      void (async () => {
        // Degrade a dead diff base to trunk before handing it to the user's
        // diff command — a stack-on-stack parent whose branch was merged +
        // cleaned would otherwise make `<deadref>...HEAD` error in the
        // session. Mirrors the render diff/sync paths' `effectiveBaseOrTrunk`.
        const base = await effectiveBaseOrTrunk(cwd, rawBase);
        diffLog.event.info(`opening diff vs ${base} (F11 to detach)`);
        const result = await enterDiffSession({ renderer, slug, cwd, base });
        if (result.kind === "spawn-failed") {
          diffLog.event.err(`diff failed to start: ${result.reason}`);
          toast(`diff failed: ${result.reason}`, theme.err, 3000);
        } else if (result.kind === "detached") {
          diffLog.event.info(`detached from diff (${slug})`);
        } else {
          diffLog.event.info(`diff exited (${result.code ?? "?"})`);
          if (result.stderr) diffLog.event.err(result.stderr);
        }
      })();
      return;
    }
    // Shift+F10 — kill-confirm for the selected worktree's shell
    // session. Mirrors Shift+F11/F12. No-op (with a hint) when
    // there's no session. Killing terminates any background
    // processes the user launched in the shell.
    if (
      k.name === "f10" &&
      k.shift &&
      !k.ctrl &&
      !k.option &&
      !k.super &&
      !k.hyper &&
      !k.meta
    ) {
      if (!current) {
        toast("select a worktree first", theme.warn, 1500);
        return;
      }
      const slug = current.wt.slug;
      if (!activeShellSessions.has(slug)) {
        toast(`no shell session on ${slug}`, theme.fgDim, 1500);
        return;
      }
      setModal({ kind: "killSessionConfirm", slug, sessionKind: "shell" });
      return;
    }
    // Shift+F11 — kill-confirm for the selected worktree's diff
    // session. Mirrors Shift+F12. No-op (with a hint) when there's no
    // session. Killing throws away gitu's scroll/expansion state, so
    // next F11 opens fresh.
    if (
      k.name === "f11" &&
      k.shift &&
      !k.ctrl &&
      !k.option &&
      !k.super &&
      !k.hyper &&
      !k.meta
    ) {
      if (!current) {
        toast("select a worktree first", theme.warn, 1500);
        return;
      }
      const slug = current.wt.slug;
      if (!activeDiffSessions.has(slug)) {
        toast(`no diff session on ${slug}`, theme.fgDim, 1500);
        return;
      }
      setModal({ kind: "killSessionConfirm", slug, sessionKind: "diff" });
      return;
    }
    // Shift+F12 — open the harness selector for a fresh spawn.
    // Replaces the old "auto-name new claude" semantics; the user now
    // picks which harness to spawn (claude / codex / opencode). The
    // claude option preserves the prior auto-name behavior (see the
    // `harnessSelect` handler above).
    if (
      k.name === "f12" &&
      k.shift &&
      !k.ctrl &&
      !k.option &&
      !k.super &&
      !k.hyper &&
      !k.meta
    ) {
      if (!current) {
        toast("select a worktree first", theme.warn, 1500);
        return;
      }
      const slug = current.wt.slug;
      if (current.status.kind === StatusKind.Busy) {
        toast(`${slug} is busy`, theme.warn, 2000);
        return;
      }
      // Default highlight = current primary, so the muscle-memory
      // "Shift+F12, F12 again" path spawns whatever TAB selected.
      const initialIdx = HARNESSES.findIndex((h) => h.id === primaryHarness);
      setModal({
        kind: "harnessSelect",
        slug,
        index: initialIdx >= 0 ? initialIdx : 0,
      });
      return;
    }
    // Ctrl+D — gracefully close the selected row's F12-target session
    // (the one the list glyph shows) by typing the harness's own exit
    // gesture into the pane: ctrl+d twice, exactly what you'd press
    // inside claude to end the convo. The conversation persists and is
    // F12-resumable; the hard `; x` kill stays for stuck sessions.
    if (k.ctrl && k.name === "d" && !k.shift && !k.option && !k.meta) {
      if (!current) {
        toast("select a worktree first", theme.warn, 1500);
        return;
      }
      const target = currentHarnessSessions.f12Target;
      if (!target?.isLive) {
        toast("no live session to close", theme.fgDim, 1500);
        return;
      }
      const slug = current.wt.slug;
      const label = getHarness(target.harnessId).label;
      createLogger(slug).event.info(`closing ${label} session (ctrl+d ×2)`);
      void closeHarnessSessionGracefully(
        slug,
        target.harnessId,
        target.extras.managedName,
      ).then(
        // The exit isn't instant (the harness shuts down, then tmux
        // reaps the session) — nudge the poll shortly after instead of
        // immediately, so the glyph flips without waiting a full tick.
        () => setTimeout(() => void refreshTmuxSessions(), 800),
        (err) => reportActionError("close session", err),
      );
      return;
    }
    // F12 — toggle into the selected worktree's "F12 target" harness
    // session. Target = most-recently-active live session across any
    // harness; if nothing live, the primary's most-recent dead
    // session; if nothing at all, spawn primary fresh. tmux's
    // `new-session -A` makes attach-or-create idempotent. From inside
    // the session, the wt-private tmux config binds F12 to
    // detach-client so the same physical key flips between contexts.
    // Refuse on busy worktrees so we don't race a destroy.
    if (
      k.name === "f12" &&
      !k.shift &&
      !k.ctrl &&
      !k.option &&
      !k.super &&
      !k.hyper &&
      !k.meta
    ) {
      if (!current) {
        toast("select a worktree first", theme.warn, 1500);
        return;
      }
      const slug = current.wt.slug;
      if (current.status.kind === StatusKind.Busy) {
        toast(`${slug} is busy`, theme.warn, 2000);
        return;
      }
      const target = currentHarnessSessions.f12Target;
      if (target) {
        // Mirror the picker's commitRow logic: synthetic placeholders
        // ride the live slot (no resume id, no kill), real live entries
        // attach, and dead codex/opencode entries need `freshSlot` to
        // displace whatever's in the shared slot — without it, the
        // resume argv is silently ignored.
        const isSyntheticLive = isSyntheticLiveSessionId(target.sessionId);
        const resumeSessionId =
          target.isLive || isSyntheticLive ? null : target.sessionId;
        const freshSlot =
          getHarness(target.harnessId).singleSlot && resumeSessionId !== null;
        doEnterHarnessSession(slug, target.harnessId, {
          managedName: target.extras.managedName,
          resumeSessionId,
          freshSlot,
        });
      } else {
        // No discoverable session — spawn primary fresh.
        doEnterHarnessSession(slug, primaryHarness, {});
      }
      return;
    }
    // TAB — fold/unfold the section under the cursor. A folded section
    // collapses to one selectable header line with a stack/section summary
    // in the detail pane. (Shift+Tab cycles the primary harness, below.)
    if (
      k.name === "tab" &&
      !k.shift &&
      !k.ctrl &&
      !k.option &&
      !k.super &&
      !k.hyper &&
      !k.meta
    ) {
      // Land the cursor sensibly across the async reflow: unfolding → the
      // section's first row; folding → the new header line. Only active
      // (non-archived) sections fold — the archived block stays flat.
      const item = currentItem;
      if (item?.kind === "section") {
        const first = item.rows[0];
        setSel(first ? first.wt.slug : `section:${item.sectionKey}`);
        void toggleSectionFold(item.sectionKey);
        return;
      }
      if (item?.kind === "wt" && !item.row.archived) {
        // Inbox rows fold too — under the sentinel key, mirroring the
        // activeItems builder.
        const key = item.row.section ?? GROUP_INBOX;
        setSel(`section:${key}`);
        void toggleSectionFold(key);
        return;
      }
      toast("no section here to fold", theme.fgDim, 1500);
      return;
    }
    // Review-request rows: a tiny set of PR-only keybinds, no
    // worktree-keyed actions. Unmapped keys fall through to the wt
    // per-row block below, which is gated on `current` (undefined for
    // a PR selection) and silently no-ops.
    if (selectedPr) {
      const prLog = createLogger("[review]");
      if (isPlainLetter(k, "p") || k.name === "return") {
        openPrUrl(selectedPr.url, selectedPr.number, null, "[review]");
        return;
      }
      if (isPlainLetter(k, "l")) {
        rememberPrTargetChord("linear");
        return;
      }
      // `w` — check out this PR's branch as a worktree in "Reviews".
      if (isPlainLetter(k, "w")) {
        const branch = selectedPr.headRefName;
        if (!branch) {
          prLog.event.warn(`review #${selectedPr.number} has no branch name`);
          toast("PR has no branch to check out", theme.warn, 2500);
          return;
        }
        setModal({
          kind: "confirm",
          pendingKey: "review-wt",
          reviewBranch: branch,
          title: "create review worktree",
          message: `Create a worktree for ${branch} and add it to "${REVIEW_SECTION}"?`,
          confirmLabel: "create",
        });
        return;
      }
    }

    // Per-row actions.
    if (!current) return;
    const rowLog = createLogger(current.wt.slug);
    if (isPlainLetter(k, "o")) {
      openInZed(current.wt.path);
      rowLog.event.info("opened in zed");
      return;
    }
    if (isPlainLetter(k, "p")) {
      if (!current.pr) {
        rowLog.event.warn("no PR for this branch");
        return;
      }
      openPrUrl(current.pr.url, current.pr.number, null, current.wt.slug);
      return;
    }
    if (isPlainLetter(k, "i")) {
      const url = linearUrlForSlug(current.wt.slug);
      if (!url) {
        rowLog.event.warn("no linear id in slug");
        return;
      }
      void openUrlHidingAlacritty(url);
      rowLog.event.info("opened linear");
      return;
    }
    if (isPlainLetter(k, "s")) {
      if (!current.fields.deploy.data) {
        rowLog.event.warn("not deployed");
        return;
      }
      const url = stageUrl(current.wt.stage);
      if (!url) {
        rowLog.event.warn("no stage domain configured");
        return;
      }
      void openUrlHidingAlacritty(url);
      rowLog.event.info(`opened ${current.wt.stage}`);
      return;
    }
    if (k.sequence === "y") {
      setModal({ kind: "yank" });
      return;
    }
    if (isPlainLetter(k, "d")) {
      if (current.status.kind === StatusKind.Busy) {
        const label = current.status.op ?? current.status.label;
        toast(`${current.wt.slug} is ${label}`, theme.warn, 2000);
        return;
      }
      // Surface the same conditions `doRemove` guards on, at prompt
      // time. When any of them would refuse the destroy, switch the
      // confirm to a force-variant so the user can opt into the
      // destructive path inline instead of bouncing out to the shell.
      const dirtyData = current.fields.dirty.data;
      const syncData = current.fields.sync.data;
      const dirty = dirtyData?.length ?? 0;
      const ahead = syncData?.remote?.ahead ?? 0;
      const reasons: string[] = [];
      if (dirty > 0) {
        reasons.push(`${dirty} uncommitted file${dirty === 1 ? "" : "s"}`);
      }
      if (ahead > 0) {
        reasons.push(`${ahead} unpushed commit${ahead === 1 ? "" : "s"}`);
      }
      // Unknown ≠ clean: while dirty/sync are still loading the checks
      // above can't clear the row, and doRemove's non-force guard would
      // refuse anyway — offer the force variant with an honest caveat so
      // the user can proceed deliberately or wait a beat and re-prompt.
      const stateUnknown = dirtyData === undefined || syncData === undefined;
      if (reasons.length > 0 || stateUnknown) {
        const lost =
          reasons.length > 0 ? `${reasons.join(", ")} will be lost.` : null;
        const caveat = stateUnknown
          ? "Dirty/unpushed state hasn't loaded yet — anything uncommitted or unpushed will be lost."
          : null;
        setModal({
          kind: "confirm",
          pendingKey: "d!",
          title: "force remove",
          message: `Force remove ${current.wt.slug}?`,
          detail: [lost, caveat].filter(Boolean).join(" "),
          confirmLabel: "remove",
          danger: true,
        });
      } else {
        setModal({
          kind: "confirm",
          pendingKey: "d",
          title: "remove worktree",
          message: `Remove ${current.wt.slug}?`,
          confirmLabel: "remove",
          danger: true,
        });
      }
      return;
    }
    if (isPlainLetter(k, "v")) {
      void openReviewerPicker(current.wt.slug);
      return;
    }
    if (isPlainLetter(k, "e")) {
      if (!current.pr) {
        toast("no PR for this row", theme.warn, 2000);
        return;
      }
      if (current.pr.state !== "OPEN") {
        toast("PR is not open", theme.warn, 2000);
        return;
      }
      if (!current.pr.isDraft) {
        toast("PR is already ready", theme.info, 2000);
        return;
      }
      setModal({
        kind: "confirm",
        pendingKey: "e",
        title: "mark ready",
        message: `Mark #${current.pr.number} ready for review?`,
        confirmLabel: "mark ready",
      });
      return;
    }
    if (k.sequence === "E") {
      if (!current.pr) {
        toast("no PR for this row", theme.warn, 2000);
        return;
      }
      if (current.pr.state !== "OPEN") {
        toast("PR is not open", theme.warn, 2000);
        return;
      }
      const reviewer = config.github.defaultReviewer;
      const steps: string[] = [];
      if (current.pr.isDraft) steps.push("mark ready");
      if (reviewer && !current.pr.requestedReviewers.includes(reviewer))
        steps.push(`request ${reviewer}`);
      if (!current.pr.autoMerge) steps.push("arm auto-merge");
      if (steps.length === 0) {
        toast(`#${current.pr.number} already shipped`, theme.info, 2000);
        return;
      }
      setModal({
        kind: "confirm",
        pendingKey: "E",
        title: "ship PR",
        message: `Ship #${current.pr.number}? (${steps.join(", ")})`,
        confirmLabel: "ship",
      });
      return;
    }
    if (isPlainLetter(k, "m")) {
      if (!current.pr) {
        toast("no PR for this row", theme.warn, 2000);
        return;
      }
      if (current.pr.state !== "OPEN") {
        toast("PR is not open", theme.warn, 2000);
        return;
      }
      // Toggle: if already armed, the same key prompts to disable.
      if (current.pr.autoMerge) {
        setModal({
          kind: "confirm",
          pendingKey: "m-",
          title: "disable auto-merge",
          message: `Disable auto-merge for #${current.pr.number}?`,
          confirmLabel: "disable",
        });
        return;
      }
      setModal({
        kind: "confirm",
        pendingKey: "m+",
        title: "merge when ready",
        message: `Enable merge-when-ready for #${current.pr.number}?`,
        confirmLabel: "enable",
      });
      return;
    }
    if (isPlainLetter(k, "f")) {
      // Tail the failing PR's `--log-failed` CI logs into the activity
      // pane. The flow refuses cleanly when checks aren't red.
      void doTailFailedChecks(current.wt.slug);
      return;
    }
    if (isPlainLetter(k, "a")) {
      const slug = current.wt.slug;
      toggleArchived(slug).then(
        ({ archived }) => {
          rowLog.event.info(archived ? "archived" : "restored from archive");
          toast(archived ? `archived ${slug}` : `restored ${slug}`, theme.info, 2000);
        },
        (err) => reportActionError("archive", err),
      );
      return;
    }
    if (isPlainLetter(k, "l")) {
      rememberPrTargetChord("linear");
      openSectionPicker();
      return;
    }
    if (isPlainLetter(k, "b")) {
      openBasePicker();
      return;
    }
    if (isPlainLetter(k, "t")) {
      if (!config.ai) {
        toast("AI summary not configured", theme.warn, 2000);
        return;
      }
      if (current.status.kind === StatusKind.Busy) {
        toast(`${current.wt.slug} is busy`, theme.warn, 2000);
        return;
      }
      const slug = current.wt.slug;
      void (async () => {
        const ok = await refreshAiSummary(slug);
        if (ok) rowLog.event.dim("regenerating AI summary");
        else toast("no diff context yet", theme.warn, 2000);
      })();
      return;
    }
}
