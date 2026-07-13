import { useMemo, useRef, useState } from "react";
import { useIsFetching } from "@tanstack/react-query";
import { useKeyboard, useRenderer, useTerminalDimensions } from "@opentui/react";
import type { KeyEvent, ScrollBoxRenderable } from "@opentui/core";

import { createLogger } from "../core/logger.ts";
import { useWtActions } from "../state/index.ts";

import { Details } from "./panels/details.tsx";
import { Footer, type FooterMode } from "./panels/footer.tsx";
import { OutputViewer } from "./panels/output-viewer.tsx";
import { RefreshWave } from "./spinner.tsx";
import { WorktreeList, type ListScrollHandle } from "./panels/list.tsx";
import { RemovedList } from "./panels/removed-list.tsx";
import { usePrimaryHarness } from "./hooks/usePrimaryHarness.ts";
import {
  useActiveSessionsBySlug,
  useHarnessSessions,
} from "./hooks/useHarnessSessions.ts";
import type { Modal } from "./modal-state.ts";
import { PostFooterModals, PreFooterModals } from "./modal-host.tsx";
import { handleSimpleModalKey } from "./modal-keys/index.ts";
import { useAction, useActionVisible, useActiveActions } from "./hooks/useAction.ts";
import { useActionDispatch } from "./hooks/useActionDispatch.ts";
import {
  useActiveDiffSessions,
  useActiveHarnessSessions,
  useActiveShellSessions,
  useClaudeSessionsBySlug,
} from "./hooks/useActiveSessions.ts";
import { useAutoCopy } from "./hooks/useAutoCopy.ts";
import { useLogTails } from "./hooks/useLogTails.ts";
import { usePaste } from "./hooks/usePaste.ts";
import { useTerminalFocus } from "./hooks/useTerminalFocus.ts";
import { useWorktreeRows } from "./hooks/useWorktreeRows.ts";
import { useStackSections } from "./hooks/useStackSections.ts";
import { useVisualItems } from "./hooks/useVisualItems.ts";
import { useAutomations } from "./hooks/useAutomations.ts";
import { useSectionDetail } from "./hooks/useSectionDetail.ts";
import { useSessionTailReconcile } from "./hooks/useSessionTailReconcile.ts";
import { useOutputFocus } from "./hooks/useOutputFocus.ts";
import {
  isCleanCandidate,
  isPlainLetter,
  printableMultiline,
  printableText,
} from "./app-helpers.ts";
import { handleFooterInputKey } from "./keyboard/footer-input-keys.ts";
import { handleGlobalKey } from "./keyboard/global-keys.ts";
import { handleNormalKey } from "./keyboard/normal-keys.ts";
import { handleRemovedViewKey } from "./keyboard/removed-view-keys.ts";
import { makeActionPickerFlows } from "./flows/action-picker.ts";
import { makeBaseFlows } from "./flows/base.ts";
import { makeDestroyFlows } from "./flows/destroy.ts";
import { makeGithubPrFlows } from "./flows/github-pr.ts";
import { makeWorktreeCreateFlows } from "./flows/new-worktree.ts";
import { makeReviewerFlows } from "./flows/reviewers.ts";
import { makeSectionFlows } from "./flows/sections.ts";
import { makeSessionFlows } from "./flows/sessions.ts";
import { usePrTargetChord } from "./hooks/usePrTargetChord.ts";
import { useRemovedView } from "./hooks/useRemovedView.ts";
import { useSessionsPickerData } from "./hooks/useSessionsPickerData.ts";
import { PrimaryHarnessBadge, UsageBadge } from "./usage-badge.tsx";
import { writeClipboard } from "../core/macos.ts";
import { theme } from "./theme.ts";

const appLog = createLogger("[app]");

export type TuiExit = { kind: "quit" };

type Props = {
  onExit: (e: TuiExit) => void;
};

export function App({ onExit }: Props) {
  const { width, height } = useTerminalDimensions();
  const renderer = useRenderer();
  const { rows, isLoading } = useWorktreeRows();
  const {
    refreshAll,
    refreshStale,
    refreshOrigin,
    refreshGithub,
    refreshTmuxSessions,
    optimisticRemoveClaude,
    fetchContributors,
    fetchMe,
    clearAll,
    invalidateWorktree,
    refreshStack,
    refreshAiSummary,
    refreshClaudeSummaries,
    toggleArchived,
    archive,
    setSection,
    setBase,
    swapOrder,
    placeSlug,
    renameSection,
    moveGroupPast,
    toggleSectionFold,
    toggleAutomationsPaused,
    toggleStackAutomationsPaused,
    mutate,
    cyclePrimaryHarness,
    refreshHarnessSessions,
  } = useWtActions();
  const primaryHarness = usePrimaryHarness();
  // Cursor is tracked by a stable key (slug, folded section, or PR URL), not an
  // index. The visual list hook resolves that key against the current rows.
  const [sel, setSel] = useState<string | null>(null);
  // In-flight restack keys (a stack's id, or a standalone worktree's
  // branch) — guards the `R` replay action against re-entry on the SAME
  // chain while letting different chains restack concurrently (the
  // engine's per-slug flocks are the real locks; this just avoids
  // spamming them from the UI). Keys are removed in `doRestackStack`'s
  // finally.
  const restackBusyRef = useRef<Set<string>>(new Set());
  // Inner scrollbox of the details pane (worktree or review-request
  // body, whichever is mounted). PageUp/PageDown page it from the
  // global key handler so tall panes that overflow the viewport stay
  // readable instead of garbling.
  const detailsScrollRef = useRef<ScrollBoxRenderable>(null);
  // Scroll-to-edge control for the list pane, called by j/k at the boundary.
  const listScrollHandleRef = useRef<ListScrollHandle | null>(null);
  const [footer, setFooter] = useState<FooterMode>({ kind: "legend" });
  // All modal/overlay state collapsed into one discriminated union so
  // the "only one modal is open at a time" invariant is structural
  // rather than emergent. Per-modal payload (cursor index, picker
  // items, slug context) lives on its variant. The keyboard handler
  // and JSX both `switch` on `modal.kind`.
  const [modal, setModal] = useState<Modal | null>(null);
  // Last section the user moved a row into. Used to default the
  // section-picker cursor — the common case is "moving several
  // adjacent worktrees into the same section", and re-aiming on
  // every open eats keystrokes. Reset to `null` on rename so the
  // sticky target doesn't dangle.
  const [lastMoveTarget, setLastMoveTarget] = useState<string | null>(null);
  // Section the user is renaming, if any. Sits alongside the footer
  // input mode (footer carries the prompt + value, this carries the
  // identity of the thing being renamed). Not folded into `modal`
  // because the rename UX uses the footer, not an overlay.
  const [pendingRename, setPendingRename] = useState<string | null>(null);
  const toastTimer = useRef<Timer | null>(null);

  // Auto-tail every busy worktree so logs surface in the activity pane
  // without user intervention. Returns the active set so rows can flag
  // a visual "is tailing" hint.
  const activeTails = useLogTails(rows);

  // Mouse-select anywhere → auto-copy on release.
  useAutoCopy();

  // Refocusing the terminal window refetches any observed query that
  // has crossed its staleTime — cheap and idempotent. Fresh data stays
  // put; there's no `git fetch origin` or full invalidation (that's
  // still `r`). Matches how the rest of the TUI treats user input:
  // "looking at it" counts as engagement that can freshen stale data.
  useTerminalFocus(() => {
    refreshStale();
  });

  // Bracketed paste → append into whichever text mode is active. No-op
  // in legend/toast/confirm modes since paste only makes sense when the
  // user is typing.
  usePaste((text) => {
    if (modal?.kind === "actionPicker" && modal.state.mode === "edit") {
      const clean = printableMultiline(text);
      if (!clean) return;
      setModal({
        ...modal,
        state: { ...modal.state, extras: modal.state.extras + clean },
      });
      return;
    }
    if (modal?.kind === "argPicker" && modal.input !== null) {
      // Single-line input — strip newlines so a paste of "acme-123\n"
      // (common from terminal selection) doesn't auto-submit or leave
      // a trailing newline in the substituted `{{arg}}`.
      const clean = printableText(text);
      if (!clean) return;
      setModal({ ...modal, input: (modal.input ?? "") + clean });
      return;
    }
    const clean = printableText(text);
    if (!clean) return;
    if (footer.kind === "input") {
      setFooter({ ...footer, value: footer.value + clean });
    }
  });

  const { wtStateForStacks, foldedSections, stackSectionLabels } =
    useStackSections(rows);

  const cleanCandidates = useMemo(
    () => rows.filter((r) => isCleanCandidate(r)),
    [rows],
  );

  // Removed-worktrees history view (`h` toggles the left pane into it).
  const {
    removedView,
    setRemovedView,
    setRemovedIndex,
    removedEntries,
    removedCursor,
    currentRemoved,
  } = useRemovedView({ rows, wtState: wtStateForStacks.data });

  const {
    activeItems,
    archivedRows,
    reviewRequestRows,
    visualItems,
    cursorIndex,
    currentItem,
    current,
    selectedPr,
    selectedSection,
  } = useVisualItems({
    rows,
    foldedSections,
    stackSectionLabels,
    selectedKey: sel,
  });

  // `g p` / `l p` PR-target chord — extracted to
  // `hooks/usePrTargetChord.ts`.
  const { rememberPrTargetChord, openPrUrl, consumePrTargetChord } =
    usePrTargetChord({ selectedPr, current });

  const listWidth = Math.max(32, Math.min(52, Math.floor(width * 0.44)));
  // Middle (list + details) is capped; activity absorbs the rest. Title
  // and footer take 1 row each, so `height - 2` is the usable column
  // budget split between middle and activity.
  const middleMax = 20;
  const activityHeight = Math.max(7, height - 2 - middleMax);

  // Action runtime state for the *selected* worktree. `currentRun`
  // drives the activity-pane swap (showing the streamed claude output
  // in place of events) and the `!`-key dispatch (open kill-confirm
  // when running, open picker otherwise).
  const currentSlug = current?.wt.slug;
  const currentRun = useAction(currentSlug);
  // Per-current-row harness session discovery: combines per-harness
  // discoverSessions queries with the live tmux name set. The hook
  // fans out three queries unconditionally (so the call is stable
  // across cursor moves) but each is `enabled: false` when wtPath is
  // empty, so cursor-on-a-PR / cursor-on-empty costs nothing.
  const currentHarnessSessions = useHarnessSessions(
    current?.wt.slug ?? "",
    current?.wt.path ?? "",
    primaryHarness,
  );
  const showActionViewer = useActionVisible(currentSlug);
  // Set of slugs whose action is in flight RIGHT NOW (no recent-window
  // tail). Drives the leftmost cluster glyph in `WorktreeList` so the
  // user has at-a-glance awareness of what's running on rows they're
  // not currently viewing.
  const activeActions = useActiveActions();
  // Per-slug list of live claude session names (`null` = primary).
  // Drives the tail-registry reconcile, the sessions picker, and the
  // auto-output focus rule.
  const claudeSessionsBySlug = useClaudeSessionsBySlug();
  // Per-slug "active session" for the list-pane harness glyph: the
  // harness + derived state F12 would attach to, computed for EVERY
  // worktree through the same `computeHarnessSessions` rule the
  // current-row hook, the details-pane AI row, and the F12 keybind use.
  // This is the single source of truth — the list glyph can't drift from
  // what F12 does or what the details pane shows. Fans session discovery
  // across all worktrees (cached at the query layer); codex/opencode get
  // state tinting too, not just the brand color.
  const sessionWorktrees = useMemo(
    () => rows.map((r) => ({ slug: r.wt.slug, path: r.wt.path })),
    [rows],
  );
  const activeSessionBySlug = useActiveSessionsBySlug(
    sessionWorktrees,
    primaryHarness,
  );
  const sectionDetail = useSectionDetail({
    selectedSection,
    wtState: wtStateForStacks.data,
    activeActions,
    activeSessionBySlug,
  });
  // Sessions-picker derived data (row list + summaries) — extracted to
  // `hooks/useSessionsPickerData.ts`.
  const { pickerRows, pickerSummaries } = useSessionsPickerData({
    modal,
    rows,
    currentHarnessSessions,
  });

  // Parallel set for diff sessions — used by the Shift+F11 hint so
  // the kill-confirm only opens when there's something to kill.
  const activeDiffSessions = useActiveDiffSessions();
  // Same for shell sessions, gating Shift+F10.
  const activeShellSessions = useActiveShellSessions();
  // Live codex/opencode slots — drive the harness-tail reconcile so the
  // bottom pane tails their rollout/SQLite trail like the claude jsonl.
  const activeCodexSessions = useActiveHarnessSessions("codex");
  const activeOpencodeSessions = useActiveHarnessSessions("opencode");

  useSessionTailReconcile({
    rows,
    claudeSessionsBySlug,
    activeShellSessions,
    activeCodexSessions,
    activeOpencodeSessions,
    activeDiffSessions,
    refreshTmuxSessions,
  });

  const {
    visibleOutputs,
    displayedOutput,
    focusedOutputId,
    setFocus,
  } = useOutputFocus({
    rows,
    currentSlug,
    currentRun,
    showActionViewer,
    claudeSessionsBySlug,
    activeSessionBySlug,
  });

  // Action launch + completion dispatch — extracted to
  // `hooks/useActionDispatch.ts`. Subscribes once to the action
  // registry (affects-tag invalidations, arg-history refinement) and
  // returns `launchAction`.
  const { launchAction } = useActionDispatch({
    rows,
    primaryHarness,
    toast,
    setFocus,
    invalidateWorktree,
    refreshOrigin,
    refreshGithub,
    refreshStack,
  });

  function toast(message: string, color = theme.ok, ms = 2500): void {
    if (toastTimer.current) clearTimeout(toastTimer.current);
    setFooter({ kind: "toast", message, color });
    toastTimer.current = setTimeout(() => {
      setFooter((f) => (f.kind === "toast" ? { kind: "legend" } : f));
      toastTimer.current = null;
    }, ms);
  }

  function quit(): void {
    onExit({ kind: "quit" });
  }

  /**
   * Standard error reporter for state-mutation chains. Disk writes
   * inside `wtstate.ts` can throw on EACCES / ENOSPC; we surface as
   * an event log line + toast.
   */
  function reportActionError(label: string, err: unknown): void {
    const msg = err instanceof Error ? err.message : String(err);
    appLog.event.err(`${label} failed: ${msg}`);
    toast(`${label} failed: ${msg}`, theme.err, 3000);
  }

  // Section-management flows (Shift+J/K moves, the section picker,
  // rename) — extracted to `flows/sections.ts`. Rebuilt per render so
  // the closures see fresh rows / selection / wtstate.
  const { doShiftMove, openSectionPicker, commitSectionPick, openSectionRename } =
    makeSectionFlows({
      rows,
      current,
      selectedSection,
      wtState: wtStateForStacks.data,
      lastMoveTarget,
      setLastMoveTarget,
      setModal,
      setFooter,
      setPendingRename,
      toast,
      reportActionError,
      setSection,
      placeSlug,
      swapOrder,
      moveGroupPast,
    });

  // Fork-base picker flow (`b`) — extracted to `flows/base.ts`.
  const { openBasePicker, commitBasePick } = makeBaseFlows({
    rows,
    current,
    setModal,
    toast,
    reportActionError,
    setBase,
  });

  // Destroy / clean / restack flows — extracted to `flows/destroy.ts`.
  // Rebuilt per render so the closures see fresh rows / selection.
  const { doRemove, doClean, doCleanSlugs, doReplayStack, doRestackStack, isRestackBusy } = makeDestroyFlows({
    rows,
    current,
    toast,
    archive,
    refreshTmuxSessions,
    invalidateWorktree,
    refreshAll,
    refreshGithub,
    restackBusyRef,
  });

  // Automated actions — evaluates `[[automations]]` triggers against
  // the same row state the panes render and dispatches through
  // `launchAction` / the clean flow / the algorithmic restack. Inert
  // (no fires, no timers beyond a cheap early return) when the config
  // defines no rules.
  const automations = useAutomations({
    rows,
    activeSessionBySlug,
    launchAction,
    doCleanSlugs,
    doRestackStack,
    isRestackBusy,
  });

  // Harness-session flows — extracted to `flows/sessions.ts`. Rebuilt
  // per render so the closures see fresh rows / primary harness.
  const {
    doEnterHarnessSession,
    doEnterSlotSession,
    doSpawnNamedClaudeSession,
    doKillClaudeSession,
  } = makeSessionFlows({
    rows,
    renderer,
    primaryHarness,
    toast,
    refreshTmuxSessions,
    refreshHarnessSessions,
    refreshClaudeSummaries,
    optimisticRemoveClaude,
  });


  /**
   * Copy `value` to the clipboard, log + toast appropriately. Used by
   * the yank chord (branch / stage / path); each item picks its own
   * label and value so the user-facing message is consistent.
   */
  function doYank(slug: string, label: string, value: string | null): void {
    const log = createLogger(slug);
    if (!value) {
      log.event.warn(`nothing to yank: ${label}`);
      toast(`no ${label} to yank`, theme.warn, 1500);
      return;
    }
    try {
      writeClipboard(value);
    } catch (err) {
      log.event.err(`pbcopy failed: ${err instanceof Error ? err.message : String(err)}`);
      log.error(err instanceof Error ? err : String(err));
      return;
    }
    log.event.info(`yanked ${label}: ${value}`);
    toast(`copied ${label}`, theme.info, 1500);
  }

  // Reviewer-picker flows (`v`) — extracted to `flows/reviewers.ts`.
  const { openReviewerPicker, submitReviewerPicker } = makeReviewerFlows({
    rows,
    modal,
    setModal,
    toast,
    fetchContributors,
    fetchMe,
    mutate,
  });

  // GitHub PR mutation flows — extracted to `flows/github-pr.ts`.
  // Rebuilt per render so the closures see fresh rows.
  const { doMarkReady, doAutoMerge, doShipPr, doTailFailedChecks } = makeGithubPrFlows({
    rows,
    toast,
    mutate,
  });

  // Action-picker helpers (`!`) — extracted to `flows/action-picker.ts`.
  const { buildActionPickerItems, canPickAction, openActionPicker } =
    makeActionPickerFlows({ rows, setModal, toast });

  // Worktree-creation flows (`n`/`N`, review checkout, removed-history
  // restore) — extracted to `flows/new-worktree.ts`.
  const { doNew, doCheckoutReview, doRestoreRemoved } = makeWorktreeCreateFlows({
    setModal,
    setSection,
    setSel,
    setRemovedView,
    refreshAll,
    toast,
  });

  // App-level keys that work in BOTH list views — extracted to
  // `keyboard/global-keys.ts`. Bound here so the removed-view and
  // normal-mode handlers share one closure and can't drift.
  const globalKey = (k: KeyEvent): boolean =>
    handleGlobalKey(k, {
      setModal,
      quit,
      refreshAll,
      setFooter,
      cleanCandidates,
      toast,
      reportActionError,
      automations,
      cyclePrimaryHarness,
      doEnterSlotSession,
    });

  // Keyboard dispatch. Layer order is load-bearing: modal swallows
  // everything → footer input → removed view → `h` toggle → normal
  // mode. The per-layer key maps live in `keyboard/` and
  // `modal-keys/`; this callback only routes.
  useKeyboard((k) => {
    // Exactly one modal is active at a time; dispatch to its handler
    // and swallow the keypress — no modal mode falls through to the
    // input/normal-mode handling below.
    if (modal) {
      if (
        handleSimpleModalKey(k, modal, {
          setModal,
          current,
          refreshTmuxSessions,
          commitBasePick,
          doYank,
          doClean,
          doRemove,
          doAutoMerge,
          doMarkReady,
          doShipPr,
          doCheckoutReview,
          doRestoreRemoved,
          clearAll,
          submitReviewerPicker,
          commitSectionPick,
          consumePrTargetChord,
          setLastMoveTarget,
          setSection,
          toast,
          reportActionError,
          visibleOutputs,
          currentSlug,
          setFocus,
          rows,
          buildActionPickerItems,
          canPickAction,
          launchAction,
          doSpawnNamedClaudeSession,
          doEnterHarnessSession,
          pickerRows,
          doKillClaudeSession,
          refreshHarnessSessions,
          refreshClaudeSummaries,
          infoColor: theme.info,
          fgDimColor: theme.fgDim,
          warnColor: theme.warn,
          logInfo: (message) => appLog.event.info(message),
          logWarn: (message) => appLog.event.warn(message),
          logErr: (message) => appLog.event.err(message),
        })
      ) {
        return;
      }
      return;
    }

    // Input mode: typing into the new-worktree prompt or the
    // rename-section prompt — every path swallows the key.
    if (footer.kind === "input") {
      handleFooterInputKey(k, {
        footer,
        setFooter,
        pendingRename,
        setPendingRename,
        renameSection,
        setLastMoveTarget,
        toast,
        doNew,
      });
      return;
    }

    // Removed-worktrees history view — its own small key map.
    if (removedView) {
      handleRemovedViewKey(k, {
        setRemovedView,
        handleGlobalKey: globalKey,
        removedEntries,
        removedCursor,
        setRemovedIndex,
        openPrUrl,
        doYank,
        setModal,
        toast,
      });
      return;
    }
    // `h` — flip the left pane to the removed-worktrees history.
    if (isPlainLetter(k, "h")) {
      setRemovedView(true);
      return;
    }

    handleNormalKey(k, {
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
      handleGlobalKey: globalKey,
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
    });
  });

  // Global in-flight count — covers the root queries and the imperative
  // `fetchOriginQuery` call, not just the observed per-worktree fields.
  // Using the per-row aggregate alone made the indicator flash briefly
  // at the tail of a refresh (after `git fetch origin` resolved) instead
  // of lighting up for the whole window.
  const fetchingCount = useIsFetching();
  const activeCount = rows.filter((r) => !r.archived).length;
  const archivedCount = rows.length - activeCount;
  // The "refreshing" signal is the animated `RefreshWave` rendered after
  // this string (width = in-flight count); the title itself stays static
  // so it doesn't re-render on every count tick. `loading...` still wins
  // during cold start — the wave is suppressed below while isLoading.
  const titleBar = useMemo(() => {
    const loadingNote = isLoading ? " · loading..." : "";
    const archivedNote = archivedCount > 0 ? ` · ${archivedCount} archived` : "";
    return ` wt · ${activeCount} worktree${activeCount === 1 ? "" : "s"}${archivedNote}${loadingNote} `;
  }, [activeCount, archivedCount, isLoading]);

  const footerHint = useMemo(() => {
    const parts: string[] = [];
    if (activeTails.size > 0) parts.push(`tailing ${activeTails.size}`);
    return parts.length > 0 ? parts.join(" · ") : undefined;
  }, [activeTails.size]);

  return (
    <box flexDirection="column" width={width} height={height} backgroundColor={theme.bg}>
      <box
        flexShrink={0}
        flexDirection="row"
        backgroundColor={theme.bgAlt}
        paddingLeft={1}
        paddingRight={1}
        height={1}
      >
        <box flexGrow={1} flexShrink={1} overflow="hidden" flexDirection="row">
          <text fg={theme.fgBright} attributes={1}>
            {titleBar}
          </text>
          <RefreshWave count={isLoading ? 0 : fetchingCount} fg={theme.fgDim} />
        </box>
        {automations.configured && automations.paused ? (
          <text fg={theme.warn}>{"auto ⏸  "}</text>
        ) : automations.pendingCount > 0 ? (
          <text fg={theme.fgDim}>{`auto ${automations.pendingCount} queued  `}</text>
        ) : null}
        <UsageBadge primary={primaryHarness} />
        <PrimaryHarnessBadge primary={primaryHarness} />
      </box>
      <box flexDirection="row" flexGrow={1}>
        {removedView ? (
          <RemovedList
            entries={removedEntries}
            selectedIndex={removedCursor}
            width={listWidth}
          />
        ) : (
          <WorktreeList
            items={activeItems}
            archivedRows={archivedRows}
            reviewRequests={reviewRequestRows}
            selectedIndex={cursorIndex}
            width={listWidth}
            activeTails={activeTails}
            activeActions={activeActions}
            activeSessionBySlug={activeSessionBySlug}
            stackSectionLabels={stackSectionLabels}
            isLoading={isLoading}
            scrollHandle={listScrollHandleRef}
          />
        )}
        <Details
          row={removedView ? undefined : current}
          reviewRequest={removedView ? undefined : selectedPr}
          section={removedView ? undefined : sectionDetail}
          removed={currentRemoved}
          width={Math.max(0, width - listWidth)}
          scrollRef={detailsScrollRef}
        />
      </box>
      <OutputViewer output={displayedOutput} height={activityHeight} />
      <PreFooterModals
        modal={modal}
        currentSlug={currentSlug}
        visibleOutputs={visibleOutputs}
        pickerRows={pickerRows}
        pickerSummaries={pickerSummaries}
      />
      <Footer mode={footer} hint={footerHint} />
      <PostFooterModals
        modal={modal}
        current={current}
        rows={rows}
        cleanCandidates={cleanCandidates}
        primaryHarness={primaryHarness}
        buildActionPickerItems={buildActionPickerItems}
      />
    </box>
  );
}
