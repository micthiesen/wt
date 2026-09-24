import { useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useQuery } from "@tanstack/react-query";
import { useKeyboard, useRenderer, useTerminalDimensions } from "@opentui/react";
import type { KeyEvent, ScrollBoxRenderable } from "@opentui/core";
import { Effect } from "effect";

import { config } from "../core/config.ts";
import { createLogger } from "../core/logger.ts";
import { markKeypress } from "../core/perf.ts";
import {
  perfSnapshotQuery,
  qk,
  remoteWorkerInfoQuery,
  remoteWorktreesQuery,
  useWtActions,
} from "../state/index.ts";
import type { RemoteWorktreeSummary } from "../core/remote-worktrees.ts";
import { remoteWorktreeLedgerKey } from "../core/worktree-ref.ts";
import { wtVersion } from "../core/update.ts";
import { worktreeActionKey } from "../core/worktree-target.ts";

import { Details } from "./panels/details.tsx";
import { Footer, type FooterMode } from "./panels/footer.tsx";
import { OutputViewer } from "./panels/output-viewer.tsx";
import { TitleBar } from "./panels/title-bar.tsx";
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
import { usePrCommentEvents } from "./hooks/usePrCommentEvents.ts";
import { useIssueStatusEvents } from "./hooks/useIssueStatusEvents.ts";
import { useDevServerEvents } from "./hooks/useDevServerEvents.ts";
import { useTerminalFocus } from "./hooks/useTerminalFocus.ts";
import { useWtStateEvents } from "./hooks/useWtStateEvents.ts";
import { useManagerReports } from "./hooks/useManagerSignals.ts";
import { useWorktreeRows } from "./hooks/useWorktreeRows.ts";
import { useStackSections } from "./hooks/useStackSections.ts";
import type { CreatedWorktreePlacement } from "./created-worktree.ts";
import { useVisualItems, visualKey } from "./hooks/useVisualItems.ts";
import { useAutomations } from "./hooks/useAutomations.ts";
import { findWorktreeModel } from "./worktree-model.ts";
import { useSectionDetail } from "./hooks/useSectionDetail.ts";
import { useSessionTailReconcile } from "./hooks/useSessionTailReconcile.ts";
import { useOutputFocus } from "./hooks/useOutputFocus.ts";
import {
  cursorSuccessor,
  isCleanCandidate,
  isPlainLetter,
  printableMultiline,
  printableText,
} from "./app-helpers.ts";
import { insertText } from "./text-edit.tsx";
import { handleFooterInputKey } from "./keyboard/footer-input-keys.ts";
import { handleGlobalKey } from "./keyboard/global-keys.ts";
import { handleNormalKey, type NormalKeysCtx } from "./keyboard/normal-keys.ts";
import { handleRemovedViewKey } from "./keyboard/removed-view-keys.ts";
import { makeActionPickerFlows } from "./flows/action-picker.ts";
import { makeBaseFlows } from "./flows/base.ts";
import { useIssueIdFlow } from "./flows/issue-id.ts";
import { makeWorkStatusFlows, type PendingStatusText } from "./flows/work-status.ts";
import { makeDestroyFlows } from "./flows/destroy.ts";
import { makeErrorFlows } from "./flows/error-report.ts";
import { makeGithubPrFlows } from "./flows/github-pr.ts";
import { makeWorktreeCreateFlows } from "./flows/new-worktree.ts";
import { makePerfFlows } from "./flows/perf-report.ts";
import { makeReviewerFlows } from "./flows/reviewers.ts";
import { makeSectionFlows } from "./flows/sections.ts";
import { makeSessionFlows } from "./flows/sessions.ts";
import { makeWorktreeMutations } from "./worktree-mutations.ts";
import { useErrorOverlayAutoPop } from "./hooks/useErrorOverlay.ts";
import { usePrTargetChord } from "./hooks/usePrTargetChord.ts";
import { useRemovedView } from "./hooks/useRemovedView.ts";
import { useSessionsPickerData } from "./hooks/useSessionsPickerData.ts";
import { writeClipboard } from "../core/macos.ts";
import { forkReported } from "./effect-boundary.ts";
import { theme } from "./theme.ts";
import { showToast } from "./toast.ts";
import {
  visibleRemoteWorktrees,
  remoteEntryKey,
  type RemoteCreation,
} from "./remote-creation.ts";
import {
  isRemoteCleanCandidate,
  type CleanCandidate,
} from "./clean-candidate.ts";

const appLog = createLogger("[app]");
const EMPTY_REMOTE_ROWS: readonly RemoteWorktreeSummary[] = [];

export type TuiExit = { kind: "quit" };

type Props = {
  onExit: (e: TuiExit) => void;
};

export function App({ onExit }: Props) {
  const { width, height } = useTerminalDimensions();
  const renderer = useRenderer();
  const { rows, githubData, archivedKeys, isLoading } = useWorktreeRows();
  const remoteWorktreeList = useQuery(remoteWorktreesQuery());
  const remoteWorkerInfo = useQuery(remoteWorkerInfoQuery());
  const remoteInventory = remoteWorktreeList.data ?? EMPTY_REMOTE_ROWS;
  const remoteUnavailable = remoteWorktreeList.isError || remoteWorkerInfo.isError;
  const remoteError =
    remoteWorkerInfo.error?.message ?? remoteWorktreeList.error?.message ?? null;
  const remoteVersionMismatch =
    !!remoteWorkerInfo.data && remoteWorkerInfo.data.build !== wtVersion();
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
    refreshAfterRemoval,
    refreshStack,
    refreshAiSummary,
    refreshClaudeSummaries,
    toggleArchived,
    dismissReviewRequest,
    archive,
    setSection,
    setBase,
    setWorkStatus,
    setIssueId,
    swapOrder,
    placeSlug,
    renameSection,
    moveGroupPast,
    toggleSectionFold,
    setSectionFolded,
    toggleAutomationsPaused,
    toggleRemovedAutomationsPaused,
    toggleStackAutomationsPaused,
    mutate,
    cyclePrimaryHarness,
    refreshHarnessSessions,
  } = useWtActions();
  const primaryHarness = usePrimaryHarness();
  // Cursor is tracked by a stable key (slug, folded section, or PR URL), not an
  // index. The visual list hook resolves that key against the current rows.
  const [sel, setSel] = useState<string | null>(null);
  const [createdSelection, setCreatedSelection] = useState<string | null>(null);
  const [createdPlacements, setCreatedPlacements] = useState<CreatedWorktreePlacement[]>([]);
  const revealCreated = useCallback((placement: CreatedWorktreePlacement) => {
    setCreatedPlacements((prev) => [...prev.filter((p) => p.key !== placement.key), placement]);
    setCreatedSelection(placement.key);
  }, []);
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
  // Footer and modal state are REF-AUTHORITATIVE: one stdin chunk can
  // parse into many key events handled in a single tick (fast typing,
  // an unbracketed paste, tmux send-keys), and React batches the
  // setState calls — so the render-closure values lag mid-burst and a
  // plain `{ ...footer, value: footer.value + ch }` per keystroke
  // keeps only the LAST character (Enter in the same chunk then
  // submits the stale-empty value and silently drops the input). The
  // setters below apply updates to the ref synchronously and mirror
  // to state for rendering; the keyboard/paste dispatchers read the
  // ref, never the render closure, so every handler's spread-and-set
  // sees the value as of THIS event, not this render.
  const [footer, setFooterState] = useState<FooterMode>({ kind: "legend" });
  const footerRef = useRef<FooterMode>(footer);
  const setFooter = useCallback(
    (f: FooterMode | ((prev: FooterMode) => FooterMode)) => {
      footerRef.current = typeof f === "function" ? f(footerRef.current) : f;
      setFooterState(footerRef.current);
    },
    [],
  );
  // Remote checkouts are absent from this machine's `git worktree list`, so
  // withhold newly discovered rows while SSH creation/install is running.
  const [remoteCreation, setRemoteCreation] = useState<RemoteCreation | null>(null);
  // All modal/overlay state collapsed into one discriminated union so
  // the "only one modal is open at a time" invariant is structural
  // rather than emergent. Per-modal payload (cursor index, picker
  // items, slug context) lives on its variant. The keyboard handler
  // and JSX both `switch` on `modal.kind`.
  const [modal, setModalState] = useState<Modal | null>(null);
  // Same ref-authoritative discipline as `footer` above.
  const modalRef = useRef<Modal | null>(modal);
  const setModal = useCallback((m: Modal | null | ((prev: Modal | null) => Modal | null)) => {
    modalRef.current = typeof m === "function" ? m(modalRef.current) : m;
    setModalState(modalRef.current);
  }, []);
  // Perf sampling runs ONLY while the `P` overlay is up — three
  // shell-outs every 2s is cheap against the load it measures, but
  // pointless (and self-defeating) with nothing watching.
  const perf = useQuery(perfSnapshotQuery({ enabled: modal?.kind === "perf" }));
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
  // The status picker's `m` pick and its `ready + verify after merge`
  // row park {slug, state, field} here while the footer input collects
  // the line; Esc there clears it (no write).
  const [pendingStatusText, setPendingStatusText] =
    useState<PendingStatusText | null>(null);
  // The `#` prompt parks its slug here while the footer collects the
  // id — same reason the status text does: typing is human-paced and
  // the cursor can move under it, so the target is frozen at open.
  const [pendingIssueSlug, setPendingIssueSlug] = useState<string | null>(null);

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
  // in legend/confirm modes since paste only makes sense when the
  // user is typing.
  usePaste((text) => {
    // Read the refs, not the render closures — see the footer/modal
    // state comment above.
    const modal = modalRef.current;
    const footer = footerRef.current;
    if (modal?.kind === "actionPicker" && modal.state.mode === "edit") {
      const clean = printableMultiline(text);
      if (!clean) return;
      setModal({
        ...modal,
        state: { ...modal.state, extras: insertText(modal.state.extras, clean) },
      });
      return;
    }
    if (modal?.kind === "argPicker" && modal.input !== null) {
      // Single-line input — strip newlines so a paste of "acme-123\n"
      // (common from terminal selection) doesn't auto-submit or leave
      // a trailing newline in the substituted `{{arg}}`.
      const clean = printableText(text);
      if (!clean) return;
      setModal({ ...modal, input: insertText(modal.input, clean) });
      return;
    }
    const clean = printableText(text);
    if (!clean) return;
    if (footer.kind === "input") {
      setFooter({ ...footer, edit: insertText(footer.edit, clean) });
    }
  });

  const { wtStateForStacks, foldedSections } = useStackSections();
  const remoteRows = useMemo(
    () =>
      visibleRemoteWorktrees(remoteCreation, remoteInventory).map((row) => ({
        ...row,
        section:
          wtStateForStacks.data?.remoteLayouts[
            remoteWorktreeLedgerKey(row.hostKey, row.slug)
          ]?.section ?? null,
      })),
    [remoteInventory, remoteCreation, wtStateForStacks.data?.remoteLayouts],
  );

  // Narrate work-status transitions (from any process) into the
  // attention feed.
  useWtStateEvents(wtStateForStacks.data, remoteRows);
  // New PR comments from other people → attention feed.
  usePrCommentEvents(rows, githubData);
  // Confirmed external task-status transitions, never optimistic labels.
  useIssueStatusEvents();
  // `wt manager report` spool → attention feed (cross-process watcher).
  useManagerReports();

  const cleanCandidates = useMemo<CleanCandidate[]>(
    () => [
      ...rows
        .filter((row) => isCleanCandidate(row))
        .map((row) => ({ kind: "local" as const, row })),
      ...remoteRows
        .filter((entry) =>
          isRemoteCleanCandidate(
            entry,
            archivedKeys.has(remoteWorktreeLedgerKey(entry.hostKey, entry.slug)),
            githubData?.prs[entry.branch],
          ),
        )
        .map((entry) => ({
          kind: "remote" as const,
          entry,
          pr: githubData?.prs[entry.branch],
        })),
    ],
    [rows, remoteRows, archivedKeys, githubData],
  );

  // Removed-worktrees history view (`h` toggles the left pane into it).
  const {
    removedView,
    setRemovedView,
    setRemovedIndex,
    removedEntries,
    removedCursor,
    currentRemoved,
    removedIssueStatuses,
  } = useRemovedView({ rows, wtState: wtStateForStacks.data });

  const activeCreatedPlacements = useMemo(() => createdPlacements.filter((placement) => {
    const state = wtStateForStacks.data;
    const layout = state?.remoteLayouts[placement.ledgerKey] ?? state?.slugs[placement.ledgerKey];
    return layout?.section === placement.section && layout.order === placement.order;
  }), [createdPlacements, wtStateForStacks.data]);

  const {
    activeItems,
    archivedItems,
    reviewRequestRows,
    visualItems,
    cursorIndex,
    currentItem,
    current,
    selectedPr,
    selectedRemote,
    selectedSection,
    currentTarget,
    selectedWorktree,
    worktrees,
  } = useVisualItems({
    rows,
    foldedSections,
    selectedKey: sel,
    remoteCreation,
    remoteWorktrees: remoteRows,
    archivedKeys,
    githubData,
    createdPlacements: activeCreatedPlacements,
    reviewRequestDismissals: wtStateForStacks.data?.reviewRequestDismissals,
  });

  // Detached dev supervisors can fail after their start command exits. The
  // shared model lets the same narrator read logs locally or through SSH.
  useDevServerEvents(worktrees);

  // A row can also leave without any wt-side action behind it — an
  // external `wt rm`, another wt instance, a branch swept by an
  // automation in a different process. `useVisualItems` holds the cursor
  // at the same visual SLOT in that case, which is the right place; this
  // adopts whatever now occupies it as the real selection. Without it
  // the selection key stays pointed at a row that no longer exists, and
  // the cursor drifts with the next re-sort instead of tracking a row.
  useEffect(() => {
    if (sel === null || !currentItem) return;
    const key = visualKey(currentItem);
    if (key !== sel) setSel(key);
  }, [sel, currentItem]);

  // Creation can finish before React observes the inventory refresh. Keep the
  // request until its actual row is visible; never select a missing key that
  // the external-removal fallback above would immediately replace.
  useEffect(() => {
    if (createdSelection === null) return;
    if (!visualItems.some((item) => visualKey(item) === createdSelection)) return;
    setSel(createdSelection);
    setCreatedSelection(null);
  }, [createdSelection, visualItems]);

  // Cursor re-aim for actions that take the selected row OUT of its slot
  // (destroy, clean sweep, archive, section move). No-op unless the
  // cursor is actually on one of `keys` — the same call is on the path
  // an automation takes, where the row under the cursor usually isn't
  // the one leaving. Rule and rationale: `cursorSuccessor`.
  const advanceCursorPast = useCallback(
    (keys: readonly string[]): void => {
      const next = cursorSuccessor(visualItems, cursorIndex, new Set(keys));
      if (next !== null) setSel(next);
    },
    [visualItems, cursorIndex],
  );

  // Set of slugs whose action is in flight RIGHT NOW (no recent-window
  // tail). Drives the leftmost cluster glyph in `WorktreeList` so the
  // user has at-a-glance awareness of what's running on rows they're
  // not currently viewing.
  const activeActions = useActiveActions();
  // Per-slug "active session" for the list-pane harness glyph: the
  // harness + derived state F12 would attach to, computed for EVERY
  // worktree through the same `computeHarnessSessions` rule the
  // current-row hook, the details-pane AI row, and the F12 keybind use.
  // The selected primary harness wins whenever it is live, so a newer
  // secondary-harness event cannot flash a different glyph.
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

  // `g p` / `l p` PR-target chord — extracted to
  // `hooks/usePrTargetChord.ts`.
  const actionSubjectFor = useCallback(
    (target: Parameters<typeof findWorktreeModel>[0]) =>
      findWorktreeModel(target, worktrees),
    [worktrees],
  );
  const { rememberPrTargetChord, openPrUrl, consumePrTargetChord } =
    usePrTargetChord({
      selectedPr,
      selectedWorktree,
    });

  const listWidth = Math.max(32, Math.min(52, Math.floor(width * 0.44)));
  // Two layouts, differing only in where the activity pane sits
  // (`[ui] activity_pane`). `column` (default): the list owns the full
  // usable height and the right column splits between details and
  // activity. `full_width`: activity spans the bottom under both panes,
  // so the list is capped to the same height as details.
  const activityInColumn = config.ui.activityPane === "column";
  const detailsMax = 20;
  // Title and footer take 1 row each, so `height - 2` is the budget the
  // details cap and the activity pane divide between them. Identical in
  // both layouts — only which box the activity height applies to moves.
  const activityHeight = Math.max(7, height - 2 - detailsMax);
  // The details pane needs its numeric height (not just flexGrow) because
  // the folded-section body sizes notes against the rows it actually has.
  const detailsHeight = Math.max(1, height - 2 - activityHeight);
  const detailsWidth = Math.max(0, width - listWidth);

  // Action runtime state for the *selected* worktree. `currentRun`
  // drives the activity-pane swap (showing the streamed claude output
  // in place of events) and the `!`-key dispatch (open kill-confirm
  // when running, open picker otherwise).
  const currentSlug = current?.wt.slug;
  const currentActionKey = currentTarget
    ? worktreeActionKey(currentTarget)
    : undefined;
  // `V`'s override of the details pane's post-merge-steps block.
  // `null` follows the row's own default (open once the check is due),
  // and it resets below on every cursor move — a display choice made
  // about one row is not a claim about the next one.
  const [verifyExpanded, setVerifyExpanded] = useState<boolean | null>(null);
  useEffect(() => setVerifyExpanded(null), [currentSlug]);
  const currentRun = useAction(currentActionKey);
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
  const showActionViewer = useActionVisible(currentActionKey);
  // Per-slug list of live claude session names (`null` = primary).
  // Drives the tail-registry reconcile, the sessions picker, and the
  // auto-output focus rule.
  const claudeSessionsBySlug = useClaudeSessionsBySlug();
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
    currentSlug: currentActionKey,
    currentRun,
    showActionViewer,
  });

  // Action launch + completion dispatch — extracted to
  // `hooks/useActionDispatch.ts`. Subscribes once to the action
  // registry (affects-tag invalidations, arg-history refinement) and
  // returns `launchAction`.
  const { launchAction, launchSlotCommand } = useActionDispatch({
    rows,
    actionSubjectFor,
    primaryHarness,
    toast,
    setFocus,
    invalidateWorktree,
    refreshOrigin,
    refreshGithub,
    refreshStack,
    refreshTmuxSessions,
    refreshRemoteWorktrees: () => remoteWorktreeList.refetch(),
  });

  // Keystroke-feedback toasts (see AGENTS.md's toast contract): a thin
  // wrapper over the module store so every flow keeps its familiar
  // `(message, color, ms)` signature. Background code toasts through
  // the logger's `{ toast: true }` opt instead — never through this.
  function toast(message: string, color = theme.ok, ms = 2500): void {
    showToast(message, color, ms);
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

  const { setSection: setSectionEffect } = makeWorktreeMutations({
    setControllerSection: setSection,
  });
  // Boundary: `setSectionEffect` is the Effect; the flows this feeds
  // (`flows/sections.ts`, `keyboard/normal-keys.ts`) still expect a
  // Promise-returning callback, so it's run here, at the composition
  // root, rather than adding a new Promise-returning core export.
  const setWorktreeSection = (
    target: Parameters<typeof setSectionEffect>[0],
    section: string | null,
  ): Promise<void> => Effect.runPromise(setSectionEffect(target, section));

  // Section-management flows (Shift+J/K moves, the section picker,
  // rename) — extracted to `flows/sections.ts`. Rebuilt per render so
  // the closures see fresh rows / selection / wtstate.
  const { doShiftMove, openSectionPicker, commitSectionPick, openSectionRename } =
    makeSectionFlows({
      rows,
      current,
      worktrees,
      currentWorktree: selectedWorktree,
      selectedSection,
      wtState: wtStateForStacks.data,
      lastMoveTarget,
      setLastMoveTarget,
      advanceCursorPast,
      setModal,
      setFooter,
      setPendingRename,
      toast,
      reportActionError,
      setWorktreeSection,
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

  // Tracker-id flow (`#`) — extracted to `flows/issue-id.ts`.
  const { openIssueIdPrompt, commitIssueId } = useIssueIdFlow({
    current,
    setFooter,
    setPendingIssueSlug,
    setIssueId,
    isSlugLive: (slug) => rows.some((r) => r.wt.slug === slug),
    toast,
  });

  // Work-status picker flow (`u`) — extracted to `flows/work-status.ts`.
  const { openStatusPicker, commitStatusPick, beginStatusNote, commitStatusText } =
    makeWorkStatusFlows({
      current,
      setModal,
      toast,
      reportActionError,
      setWorkStatus,
      setFooter,
      setPendingStatusText,
      isSlugLive: (slug) => rows.some((r) => r.wt.slug === slug),
    });

  // Destroy / clean / restack flows — extracted to `flows/destroy.ts`.
  // Rebuilt per render so the closures see fresh rows / selection.
  const {
    doRemoveWorktree,
    doClean,
    doCleanSlugs,
    doReplayStack,
    doRestackStack,
    isRestackBusy,
  } = makeDestroyFlows({
    rows,
    remoteWorktrees: remoteRows,
    remotePullRequests: githubData?.prs,
    archivedKeys,
    current,
    toast,
    archive,
    advanceCursorPast,
    refreshTmuxSessions,
    refreshAfterRemoval,
    refreshAll,
    refreshGithub,
    optimisticRemoveRemoteWorktree: (remote, slug, run) =>
      mutate<RemoteWorktreeSummary[]>({
        filter: { queryKey: qk.remoteWorktrees(remote.host) },
        patch: (prev) => prev?.filter((row) => row.slug !== slug),
        run,
      }),
    restackBusyRef,
  });

  // Automated actions — evaluates `[[automations]]` triggers against
  // the same row state the panes render and dispatches through
  // `launchAction` / the clean flow / the algorithmic restack. Inert
  // (no fires, no timers beyond a cheap early return) when the config
  // defines no rules.
  const automations = useAutomations({
    rows,
    primaryHarness,
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
    doEnterWorktreeSession,
  } = makeSessionFlows({
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
  });

  // `i` inside the perf overlay: send the snapshot to the wt-source
  // session, then enter it. Built per render so it closes over the
  // latest sample rather than whichever one was current at mount.
  const { doPerfInvestigate } = makePerfFlows({
    snapshot: perf.data,
    setModal,
    doEnterSlotSession,
    toast,
  });

  // `i` inside the error overlay — same shape as perf's investigate
  // flow, sending the newest captured error instead of a snapshot.
  const { doErrorInvestigate } = makeErrorFlows({
    setModal,
    doEnterSlotSession,
    toast,
  });

  // Pop the error overlay when the capture ring has unacknowledged
  // errors; queues behind whatever modal is already open.
  useErrorOverlayAutoPop({ modal, setModal });

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
    forkReported(
      writeClipboard(value).pipe(
        Effect.tap(() =>
          Effect.sync(() => {
            log.event.info(`yanked ${label}: ${value}`);
            toast(`copied ${label}`, theme.info, 1500);
          }),
        ),
      ),
      (error) => {
        log.event.err(`pbcopy failed: ${error.message}`);
        toast(`copy failed: ${label}`, theme.err, 3000);
      },
    );
  }

  // Reviewer-picker flows (`v`) — extracted to `flows/reviewers.ts`.
  const { openReviewerPicker, submitReviewerPicker } = makeReviewerFlows({
    rows,
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
    refreshGithub,
  });

  // Action-picker (`!`) + manager palette (`M`) + slot palette
  // (`<`/`>`/`\`) helpers — extracted to `flows/action-picker.ts`.
  const {
    buildActionPickerItems,
    buildManagerPickerItems,
    buildSlotPickerItems,
    canPickAction,
    openActionPicker,
    openManagerPalette,
    openSlotPalette,
  } = makeActionPickerFlows({ rows, actionSubjectFor, setModal, toast });

  // Worktree-creation flows (`n`/`N`, review checkout, removed-history
  // restore) — extracted to `flows/new-worktree.ts`.
  const { doNew, doRemoteNew, doCheckoutReview, doRestoreRemoved } = makeWorktreeCreateFlows({
    setModal,
    setSection,
    revealCreated,
    setSectionFolded,
    setRemovedView,
    setRemoteCreation: (creation) => {
      setRemoteCreation(creation);
      if (creation) setCreatedSelection(`remote:${remoteEntryKey(creation)}`);
    },
    remoteWorktrees: remoteRows,
    refreshAll,
    refreshRemoteWorktrees: async () => {
      const result = await remoteWorktreeList.refetch();
      return result.data ?? [];
    },
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
      openManagerPalette: () => openManagerPalette(current?.wt.slug ?? null),
      openSlotPalette,
    });

  // Keyboard dispatch. Layer order is load-bearing: modal swallows
  // everything → footer input → removed view → `h` toggle → normal
  // mode. The per-layer key maps live in `keyboard/` and
  // `modal-keys/`; this callback only routes.
  useKeyboard((k) => {
    // WT_PERF input-latency probe: stamp the keypress before any
    // dispatch work (no-op when unarmed).
    markKeypress();
    // Shadow the render-closure values with the authoritative refs —
    // several key events can land in one tick (see the footer/modal
    // state comment), and routing on a stale closure would misdispatch
    // the tail of a burst (e.g. `n` opens the prompt, the next chars
    // must route to it immediately).
    const modal = modalRef.current;
    const footer = footerRef.current;
    // Exactly one modal is active at a time; dispatch to its handler
    // and swallow the keypress — no modal mode falls through to the
    // input/normal-mode handling below.
    if (modal) {
      if (
        handleSimpleModalKey(k, modal, {
          setModal,
          current,
          selectedSection,
          refreshTmuxSessions,
          refreshPerf: () => perf.refetch(),
          doPerfInvestigate,
          doErrorInvestigate,
          commitBasePick,
          commitStatusPick,
          beginStatusNote,
          doYank,
          doClean,
          doRemoveWorktree,
          doAutoMerge: async (_slug, mode, target) => {
            const subject = target ? actionSubjectFor(target) : undefined;
            if (!subject) {
              toast("worktree gone", theme.warn, 2000);
              return;
            }
            await doAutoMerge(subject, mode);
          },
          doMarkReady,
          doShipPr,
          doCheckoutReview,
          doRestoreRemoved,
          clearAll,
          submitReviewerPicker,
          commitSectionPick,
          consumePrTargetChord,
          setLastMoveTarget,
          advanceCursorPast,
          toast,
          reportActionError,
          visibleOutputs,
          currentSlug,
          setFocus,
          rows,
          buildActionPickerItems,
          buildManagerPickerItems,
          buildSlotPickerItems,
          canPickAction,
          launchAction,
          launchSlotCommand,
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
        doRemoteNew,
        pendingStatusText,
        setPendingStatusText,
        commitStatusText,
        pendingIssueSlug,
        setPendingIssueSlug,
        commitIssueId,
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
        toggleRemovedAutomationsPaused,
      });
      return;
    }
    // `h` — flip the left pane to the removed-worktrees history.
    if (isPlainLetter(k, "h")) {
      setRemovedView(true);
      return;
    }

    const normalCtx: NormalKeysCtx = {
      selectedSection,
      focusedOutputId,
      setFocus,
      visibleOutputs,
      displayedOutput,
      current,
      currentItem,
      selectedPr,
      selectedRemote,
      selectedWorktree,
      currentTarget,
      visualItems,
      cursorIndex,
      currentSlug,
      verifyExpanded,
      setVerifyExpanded,
      setSel,
      advanceCursorPast,
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
      doEnterWorktreeSession,
      doEnterHarnessSession,
      handleGlobalKey: globalKey,
      doShiftMove,
      openSectionPicker,
      openSectionRename,
      openIssueIdPrompt,
      openBasePicker,
      openStatusPicker,
      openActionPicker,
      openReviewerPicker,
      doReplayStack,
      doTailFailedChecks,
      automations,
      toggleAutomationsPaused,
      toggleStackAutomationsPaused,
      toggleArchived,
      dismissReviewRequest,
      setWorktreeSection,
      toggleSectionFold,
      setSectionFolded,
      refreshAiSummary,
      toast,
      reportActionError,
    };

    handleNormalKey(k, normalCtx);
  });

  const remoteArchivedCount = remoteRows.filter((row) =>
    archivedKeys.has(remoteWorktreeLedgerKey(row.hostKey, row.slug)),
  ).length;
  const activeCount =
    rows.filter((r) => !r.archived).length +
    (remoteRows.length - remoteArchivedCount);
  const archivedCount = rows.filter((r) => r.archived).length + remoteArchivedCount;

  const footerHint = useMemo(() => {
    const parts: string[] = [];
    if (activeTails.size > 0) parts.push(`tailing ${activeTails.size}`);
    return parts.length > 0 ? parts.join(" · ") : undefined;
  }, [activeTails.size]);

  return (
    <box flexDirection="column" width={width} height={height} backgroundColor={theme.bg}>
      <TitleBar
        isLoading={isLoading}
        activeCount={activeCount}
        archivedCount={archivedCount}
        remoteUnavailable={remoteUnavailable}
        remoteVersionMismatch={remoteVersionMismatch}
        automationsConfigured={automations.configured}
        automationsPaused={automations.paused}
        automationsPending={automations.pendingCount}
        primaryHarness={primaryHarness}
      />
      {/* The in-flight fetch counter + refresh wave live INSIDE TitleBar
          (see its header comment): useIsFetching re-renders per fetch
          event, and at App level that re-rendered the whole tree. */}
      <box
        flexDirection="row"
        flexGrow={activityInColumn ? 1 : 0}
        height={activityInColumn ? undefined : detailsHeight}
        flexShrink={activityInColumn ? undefined : 0}
        minHeight={0}
      >
        {removedView ? (
          <RemovedList
            entries={removedEntries}
            selectedIndex={removedCursor}
            width={listWidth}
            issueStatuses={removedIssueStatuses}
          />
        ) : (
          <WorktreeList
            items={activeItems}
            archivedItems={archivedItems}
            reviewRequests={reviewRequestRows}
            selectedIndex={cursorIndex}
            width={listWidth}
            activeTails={activeTails}
            activeActions={activeActions}
            activeSessionBySlug={activeSessionBySlug}
            isLoading={isLoading}
            remoteUnavailable={remoteUnavailable}
            githubData={githubData}
            scrollHandle={listScrollHandleRef}
          />
        )}
        <box
          flexDirection="column"
          width={detailsWidth}
          flexShrink={0}
          minHeight={0}
        >
          <Details
            worktree={removedView ? undefined : selectedWorktree}
            reviewRequest={removedView ? undefined : selectedPr}
            remote={
              removedView || selectedWorktree
                ? undefined
                : selectedRemote
            }
            remoteUnavailable={remoteUnavailable}
            remoteError={remoteError}
            section={removedView ? undefined : sectionDetail}
            removed={currentRemoved}
            removedIssueStatuses={removedIssueStatuses}
            width={detailsWidth}
            height={detailsHeight}
            scrollRef={detailsScrollRef}
            sessionState={
              current
                ? activeSessionBySlug.get(current.wt.slug)?.state ?? undefined
                : undefined
            }
            verifyExpanded={verifyExpanded}
          />
          {activityInColumn && (
            <OutputViewer output={displayedOutput} height={activityHeight} />
          )}
        </box>
      </box>
      {!activityInColumn && (
        <OutputViewer output={displayedOutput} height={activityHeight} />
      )}
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
        selectedSection={selectedSection}
        rows={rows}
        cleanCandidates={cleanCandidates}
        primaryHarness={primaryHarness}
        buildActionPickerItems={buildActionPickerItems}
        buildManagerPickerItems={buildManagerPickerItems}
        buildSlotPickerItems={buildSlotPickerItems}
        actionSubjectFor={actionSubjectFor}
        perfSnapshot={perf.data}
        perfError={perf.error}
      />
    </box>
  );
}
