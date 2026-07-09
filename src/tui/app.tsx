import { useMemo, useRef, useState } from "react";
import { useIsFetching, useQuery } from "@tanstack/react-query";
import { useKeyboard, useRenderer, useTerminalDimensions } from "@opentui/react";
import type { KeyEvent, ScrollBoxRenderable } from "@opentui/core";

import {
  actionRegistry,
  BUILTIN_ACTIONS,
  evaluateActionRequirements,
} from "../core/actions.ts";
import { config, type PullRequestTarget } from "../core/config.ts";
import { createWorktree, parseInput } from "../core/lifecycle.ts";
import {
  editReviewers,
  pullRequestOpenUrl,
  pullRequestOpenUrlForTarget,
} from "../core/github.ts";
import {
  nextAutoName,
} from "../core/claude-sessions.ts";
import { linearUrlForSlug } from "../core/linear.ts";
import { effectiveBaseOrTrunk } from "../core/git.ts";
import { createLogger } from "../core/logger.ts";
import { stageUrl } from "../core/stage.ts";
import {
  closeHarnessSessionGracefully,
} from "../core/tmux.ts";
import { StatusKind } from "../core/types.ts";
import { claudeSummariesQuery, patchPullRequest, useWtActions, type GithubData } from "../state/index.ts";

import {
  ActionEditModal,
  ActionPickerModal,
  assignActionKeys,
  type PickerItem,
} from "./panels/action-picker.tsx";
import { CleanConfirmModal } from "./panels/clean-confirm.tsx";
import { ConfirmModal } from "./panels/confirm-modal.tsx";
import { Details } from "./panels/details.tsx";
import { Footer, type FooterMode } from "./panels/footer.tsx";
import { HelpOverlay } from "./panels/help.tsx";
import { KillActionConfirmModal } from "./panels/kill-action-confirm.tsx";
import { KillSessionConfirmModal } from "./panels/kill-session-confirm.tsx";
import { ArgPickerModal, MultiPickerModal, PickerModal, type MultiPickerItem } from "./panels/picker.tsx";
import { OutputsPicker } from "./panels/outputs-picker.tsx";
import { OutputViewer } from "./panels/output-viewer.tsx";
import { RefreshWave } from "./spinner.tsx";
import {
  SectionPickerModal,
  type SectionPickerItem,
} from "./panels/section-picker.tsx";
import {
  SessionsPickerList,
  SessionsPickerNew,
} from "./panels/sessions-picker.tsx";
import { WorktreeList, type ListScrollHandle } from "./panels/list.tsx";
import { YankModal } from "./panels/yank.tsx";
import { usePrimaryHarness } from "./hooks/usePrimaryHarness.ts";
import {
  isSyntheticLiveSessionId,
  useActiveSessionsBySlug,
  useHarnessSessions,
} from "./hooks/useHarnessSessions.ts";
import { getHarness, HARNESSES } from "../core/harness/index.ts";
import { HarnessPickerModal } from "./panels/harness-picker.tsx";
import type { PickerRow } from "./panels/sessions-picker.tsx";
import type { Modal } from "./modal.ts";
import { handleSimpleModalKey } from "./modal-key-handlers.ts";
import { enterDiffSession } from "./diff-session.ts";
import { enterShellSession } from "./shell-session.ts";
import { useAction, useActionVisible, useActiveActions } from "./hooks/useAction.ts";
import { useActionDispatch } from "./hooks/useActionDispatch.ts";
import {
  useActiveDiffSessions,
  useActiveHarnessSessions,
  useActiveShellSessions,
  useClaudeSessionsBySlug,
} from "./hooks/useActiveSessions.ts";
import { eventsOutputId, indexOfOutput } from "../core/outputs.ts";
import { useAutoCopy } from "./hooks/useAutoCopy.ts";
import { useLogTails } from "./hooks/useLogTails.ts";
import { usePaste } from "./hooks/usePaste.ts";
import { useTerminalFocus } from "./hooks/useTerminalFocus.ts";
import {
  GROUP_INBOX,
  STACK_SECTION_PREFIX,
  useWorktreeRows,
  type WorktreeRow,
} from "./hooks/useWorktreeRows.ts";
import { useStackSections } from "./hooks/useStackSections.ts";
import { useVisualItems, visualKey } from "./hooks/useVisualItems.ts";
import { useAutomations } from "./hooks/useAutomations.ts";
import { useSectionDetail } from "./hooks/useSectionDetail.ts";
import { useSessionTailReconcile } from "./hooks/useSessionTailReconcile.ts";
import { useOutputFocus } from "./hooks/useOutputFocus.ts";
import {
  actionSkillPrefix,
  buildActionVars,
  isCleanCandidate,
  isPlainLetter,
  isShiftedLetter,
  parseNewInput,
  printableMultiline,
  printableText,
  resolveDiffBase,
} from "./app-helpers.ts";
import { makeDestroyFlows } from "./flows/destroy.ts";
import { makeGithubPrFlows } from "./flows/github-pr.ts";
import { makeSessionFlows } from "./flows/sessions.ts";
import { PrimaryHarnessBadge, UsageBadge } from "./usage-badge.tsx";
import { openInZed, openUrlHidingAlacritty, writeClipboard } from "./helpers.ts";
import {
  DOTFILES_SLOT,
  MAIN_CLONE_SLOT,
  WT_SOURCE_SLOT,
} from "./session-slots.ts";
import { theme } from "./theme.ts";

const appLog = createLogger("[app]");
const newLog = createLogger("[new]");
const wtSourceLog = createLogger(WT_SOURCE_SLOT.label);
const PR_TARGET_CHORD_MS = 1_200;

/** Section a review-requested PR lands in when checked out via `w`. */
const REVIEW_SECTION = "Reviews";

export type TuiExit = { kind: "quit" };

type Props = {
  onExit: (e: TuiExit) => void;
};

type PendingPrTargetChord = {
  target: PullRequestTarget;
  url: string;
  number: number;
  logName: string;
  timer: Timer;
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
    mutate,
    cyclePrimaryHarness,
    refreshHarnessSessions,
  } = useWtActions();
  const primaryHarness = usePrimaryHarness();
  // Cursor is tracked by a stable key (slug, folded section, or PR URL), not an
  // index. The visual list hook resolves that key against the current rows.
  const [sel, setSel] = useState<string | null>(null);
  // Guards the `R` replay-stack action against re-entry while a rebase is
  // in flight (the engine flock is the real lock; this just avoids spamming
  // it from the UI). Reset in `doReplayStack`'s finally.
  const restackBusyRef = useRef(false);
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
  const pendingPrTargetChordRef = useRef<PendingPrTargetChord | null>(null);

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

  function clearPendingPrTargetChord(): void {
    const pending = pendingPrTargetChordRef.current;
    if (pending) clearTimeout(pending.timer);
    pendingPrTargetChordRef.current = null;
  }

  function rememberPrTargetChord(target: PullRequestTarget): boolean {
    const pr = selectedPr ?? current?.pr;
    if (!pr) return false;
    clearPendingPrTargetChord();
    const logName = selectedPr ? "[review]" : current?.wt.slug ?? "[app]";
    const timer = setTimeout(() => {
      pendingPrTargetChordRef.current = null;
    }, PR_TARGET_CHORD_MS);
    pendingPrTargetChordRef.current = {
      target,
      url: pr.url,
      number: pr.number,
      logName,
      timer,
    };
    return true;
  }

  function openPrUrl(
    url: string,
    number: number,
    target: PullRequestTarget | null,
    logName: string,
  ): void {
    const resolved = target
      ? pullRequestOpenUrlForTarget(url, target)
      : pullRequestOpenUrl(url);
    const label = target ?? config.github.prTarget;
    void openUrlHidingAlacritty(resolved);
    createLogger(logName).event.info(`opened PR #${number} in ${label}`);
  }

  function consumePrTargetChord(k: KeyEvent): boolean {
    if (!isPlainLetter(k, "p")) return false;
    const pending = pendingPrTargetChordRef.current;
    if (!pending) return false;
    clearPendingPrTargetChord();
    openPrUrl(pending.url, pending.number, pending.target, pending.logName);
    return true;
  }

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
  // LLM-authored summary snippets for the picker's currently-open
  // worktree. Only fetched when the picker is open (gated by
  // `enabled`); the queryFn does light tail-bounded disk reads
  // cached by (mtime, size) so repeat opens are essentially free.
  const pickerWt = (modal?.kind === "claudeSessionsPicker"
    ? rows.find((r) => r.wt.slug === modal.slug)?.wt
    : undefined);
  const pickerWtForQuery = pickerWt ?? { slug: "__none__", path: "" };
  const summariesQuery = useQuery({
    ...claudeSummariesQuery(pickerWtForQuery),
    enabled: !!pickerWt,
  });
  // Sessions-picker rows for the current modal slug. Built from
  // `currentHarnessSessions` so claude/codex/opencode entries surface
  // in one list. Trailing "+ new" affordances are appended one per
  // harness so per-harness letters (`c`/`o`/`x`) land on distinct
  // rows. Index space: [sessions...] [new-claude] [new-codex] [new-opencode].
  const pickerSlug =
    modal?.kind === "claudeSessionsPicker" ? modal.slug : null;
  const pickerRows = useMemo<ReadonlyArray<PickerRow>>(() => {
    if (pickerSlug === null) return [];
    // `sessions` is already sorted live-first then recency-desc by
    // `compareSessionsForDisplay` inside the hook.
    const out: PickerRow[] = currentHarnessSessions.sessions.map((entry) => ({
      kind: "session",
      entry,
    }));
    for (const h of HARNESSES) {
      out.push({ kind: "new", harnessId: h.id });
    }
    return out;
  }, [pickerSlug, currentHarnessSessions.sessions]);
  // Summaries keyed by session id for the picker's bottom panel.
  // Claude-only today; codex / opencode entries fall back to the
  // "(no summary yet)" placeholder.
  const pickerSummaries = useMemo(() => {
    const m = new Map<string, { text: string } | null>();
    const raw = summariesQuery.data ?? {};
    for (const [id, value] of Object.entries(raw)) {
      m.set(id, value);
    }
    return m;
  }, [summariesQuery.data]);

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

  /**
   * Build the section-picker item list. Excludes the current row's
   * section since "move this row to where it already is" is never
   * useful (the user explicitly asked to drop that as clutter).
   * "+ new section" sits at the bottom with `l` as its quick chord
   * trigger so `l l` creates a fresh section in two keystrokes.
   */
  function buildSectionItems(
    currentRow: WorktreeRow,
  ): SectionPickerItem[] {
    const items: SectionPickerItem[] = [];
    const currentSection = currentRow.section;
    if (currentSection !== null) items.push({ kind: "none" });
    const seen = new Set<string>();
    for (const r of rows) {
      if (r.archived) continue;
      if (r.section === null || seen.has(r.section)) continue;
      seen.add(r.section);
      if (r.section === currentSection) continue;
      // Manifest-driven stack sections aren't manually joinable — skip
      // them so the picker only lists manual named sections.
      if (r.sectionIsStack) continue;
      items.push({ kind: "section", name: r.section });
    }
    items.push({ kind: "create" });
    return items;
  }

  /**
   * Move a whole group (a stack section, a manual section, the inbox)
   * one display slot in `dir`. The landmark is the adjacent group that
   * currently RENDERS rows — `moveGroupPast` then jumps any invisible
   * group sitting in between (an empty inbox) so one keypress is one
   * visual step, never a phantom no-change move.
   */
  function doMoveGroup(groupKey: string, dir: -1 | 1, what: string): void {
    const order = wtStateForStacks.data?.sectionsOrder ?? [];
    const present = new Set<string>();
    for (const r of rows) {
      if (!r.archived) present.add(r.section ?? GROUP_INBOX);
    }
    const seq = order.filter((g) => present.has(g));
    const idx = seq.indexOf(groupKey);
    if (idx < 0) return; // unranked group mid-refresh; self-heals on next read
    const neighbor = seq[idx + dir];
    if (!neighbor) {
      toast(
        dir > 0 ? `${what} already at bottom` : `${what} already at top`,
        theme.fgDim,
        1500,
      );
      return;
    }
    moveGroupPast(groupKey, neighbor, dir > 0 ? "after" : "before").then(
      (moved) => {
        if (moved) toast(`moved ${what} ${dir > 0 ? "down" : "up"}`, theme.info, 1200);
      },
      (err) => reportActionError("move", err),
    );
  }

  /**
   * Unified Shift+J/K — moves the smallest movable thing under the
   * cursor:
   *   - A row in the inbox / a manual section: swap with its same-group
   *     neighbor, or slide to the near edge of the adjacent group across
   *     a boundary (top of next on `J`, bottom of prev on `K`). Stack
   *     sections can't be joined (membership is manifest-derived), so a
   *     sliding row hops over them in one keypress; the inbox is a valid
   *     target even when empty.
   *   - A row inside a stack section (slice or holistic origin): rows
   *     there are ordered by the manifest topology, so the move applies
   *     to the WHOLE stack — one group slot.
   *   - A folded section header (stack or manual): the whole group moves.
   * The archive boundary is hard: rows can't cross into archived via
   * J/K — that's `a`'s job.
   */
  function doShiftMove(dir: -1 | 1): void {
    if (selectedSection) {
      doMoveGroup(
        selectedSection.sectionKey,
        dir,
        selectedSection.isStack ? "stack" : "section",
      );
      return;
    }
    if (!current) return;
    if (current.archived) {
      toast("archived rows don't reorder, use `a` to restore", theme.fgDim, 1500);
      return;
    }
    if (current.sectionIsStack) {
      doMoveGroup(current.section!, dir, "stack");
      return;
    }
    const active = rows.filter((r) => !r.archived);
    const idx = active.indexOf(current);
    if (idx < 0) return;
    const slug = current.wt.slug;
    const target = active[idx + dir];
    if (target && target.section === current.section) {
      const bucket = active
        .filter((r) => r.section === current.section)
        .map((r) => r.wt.slug);
      swapOrder(slug, target.wt.slug, current.section, bucket).catch((err) =>
        reportActionError("reorder", err),
      );
      return;
    }
    // Crossing a group boundary: land at the near edge of the adjacent
    // group in the ranked sequence. Built from `sectionsOrder` rather
    // than the neighboring ROW so stack sections get skipped and the
    // inbox is reachable even when it has no rows (the only way back
    // out when every row is sectioned).
    const order = wtStateForStacks.data?.sectionsOrder ?? [];
    const present = new Set<string>();
    for (const r of active) present.add(r.section ?? GROUP_INBOX);
    const seq = order.filter((g) => g === GROUP_INBOX || present.has(g));
    const start = seq.indexOf(current.section ?? GROUP_INBOX);
    if (start < 0) return; // unranked mid-refresh; self-heals on next read
    let i = start + dir;
    while (i >= 0 && i < seq.length && seq[i]!.startsWith(STACK_SECTION_PREFIX)) {
      i += dir;
    }
    const targetGroup = seq[i];
    if (targetGroup === undefined) {
      toast(dir > 0 ? "already at bottom" : "already at top", theme.fgDim, 1500);
      return;
    }
    const sectionVal = targetGroup === GROUP_INBOX ? null : targetGroup;
    placeSlug(slug, sectionVal, dir > 0 ? "top" : "bottom").then(
      () => toast(`moved to ${sectionVal ?? "Inbox"}`, theme.info, 1200),
      (err) => reportActionError("move", err),
    );
  }

  function openSectionPicker(): void {
    if (!current) return;
    if (current.archived) {
      toast("archived rows don't have a section context, use `a` to restore", theme.fgDim, 2000);
      return;
    }
    if (current.sectionIsStack) {
      toast("stack rows are auto-managed (manifest-driven)", theme.fgDim, 1800);
      return;
    }
    const items = buildSectionItems(current);
    // Default cursor: sticky last-move-target if it's still in the
    // list (and isn't the current section), else the first item.
    // The user's most common workflow is "move several rows into the
    // same section", and forcing them to re-aim every time eats keys.
    let initial = 0;
    if (lastMoveTarget !== null && lastMoveTarget !== current.section) {
      const i = items.findIndex(
        (it) => it.kind === "section" && it.name === lastMoveTarget,
      );
      if (i >= 0) initial = i;
    }
    setModal({
      kind: "sectionPicker",
      title: `move ${current.wt.slug} to section`,
      slug: current.wt.slug,
      items,
      index: initial,
      newName: null,
    });
  }

  function commitSectionPick(item: SectionPickerItem, slug: string): void {
    if (item.kind === "none") {
      setSection(slug, null).then(
        () => toast("moved to Inbox", theme.info, 1500),
        (err) => reportActionError("move", err),
      );
      setLastMoveTarget(null);
      setModal(null);
      return;
    }
    if (item.kind === "section") {
      const target = item.name;
      setSection(slug, target).then(
        () => toast(`moved to ${target}`, theme.info, 1500),
        (err) => reportActionError("move", err),
      );
      setLastMoveTarget(target);
      setModal(null);
      return;
    }
    // "+ new section" — switch to input mode. Submission lives in the
    // keyboard handler.
    setModal((m) =>
      m?.kind === "sectionPicker" ? { ...m, newName: "" } : m,
    );
  }

  function openBasePicker(): void {
    if (!current) return;
    if (current.archived) {
      toast("archived rows have no live worktree to diff", theme.fgDim, 2000);
      return;
    }
    if (current.stack && !current.stack.isHolistic) {
      toast("stack slices get their base from the manifest — use /restack or `wt stack`", theme.fgDim, 2500);
      return;
    }
    const recorded =
      current.stackedOn?.via === "fork" ? current.stackedOn.branch : null;
    const siblings = rows
      .filter((r) => !r.archived && r.wt.slug !== current.wt.slug)
      .map((r) => r.wt.branch);
    // A recorded base whose worktree was already cleaned (branch kept)
    // wouldn't show up via the rows scan — surface it anyway so the
    // "(current)" marker is always visible.
    if (recorded && !siblings.includes(recorded)) siblings.unshift(recorded);
    const items = [
      {
        label: `none — diff against ${config.branch.base}`,
        branch: null as string | null,
      },
      ...siblings.map((b) => ({
        label: b === recorded ? `${b} (current)` : b,
        branch: b as string | null,
      })),
    ];
    const idx = recorded ? items.findIndex((it) => it.branch === recorded) : 0;
    setModal({
      kind: "basePicker",
      slug: current.wt.slug,
      items,
      index: Math.max(0, idx),
    });
  }

  function commitBasePick(
    item: { label: string; branch: string | null },
    slug: string,
  ): void {
    setModal(null);
    const row = rows.find((r) => r.wt.slug === slug);
    if (!row) return;
    setBase(row.wt, item.branch).then(
      () =>
        toast(
          item.branch
            ? `base → ${item.branch} (record only, no rebase)`
            : `base cleared — diffing against ${config.branch.base}`,
          theme.info,
          2000,
        ),
      (err) => reportActionError("set base", err),
    );
  }

  /**
   * Open the rename prompt for the current row's section. No-op for
   * unsectioned and archived rows — there's no nameable section to
   * rename in those contexts.
   */
  function openSectionRename(): void {
    if (!current || current.archived) return;
    if (current.section === null) {
      toast("the Inbox can't be renamed", theme.fgDim, 1500);
      return;
    }
    if (current.sectionIsStack) {
      toast("stack section name is auto-derived", theme.fgDim, 1500);
      return;
    }
    setPendingRename(current.section);
    setFooter({
      kind: "input",
      prompt: `rename "${current.section}":`,
      value: current.section,
      purpose: "rename-section",
    });
  }

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

  async function openReviewerPicker(slug: string): Promise<void> {
    const row = rows.find((r) => r.wt.slug === slug);
    if (!row?.pr) {
      toast("no PR for this row", theme.warn, 2000);
      return;
    }
    if (row.pr.state !== "OPEN") {
      toast("PR is not open", theme.warn, 2000);
      return;
    }
    if (row.pr.isDraft) {
      toast("PR is a draft (mark ready first)", theme.warn, 2000);
      return;
    }
    // `fetchContributors` returns cached data without awaiting when
    // warm (background refresh when stale). Only the first-ever open
    // pays a fetch; after that the picker opens instantly even when
    // the cached list is stale. `fetchMe` is process-cached after
    // first call.
    const [contributors, me] = await Promise.all([
      fetchContributors(),
      fetchMe(),
    ]);
    const requested = new Set(row.pr.requestedReviewers);
    // Three-tier candidate list:
    //   1. PR-scoped suggestions (highest signal — file ownership +
    //      history). Often empty on small diffs.
    //   2. Already-requested logins/teams not in (1), so the picker
    //      doubles as a way to *remove* them.
    //   3. Repo-wide contributors as the fallback so the picker is
    //      never empty just because (1) was. Cached for 24h.
    const items: MultiPickerItem[] = [];
    const seen = new Set<string>();
    const skipSelf = (login: string) => me !== null && login === me;
    for (const s of row.pr.suggestedReviewers) {
      if (skipSelf(s.login)) continue;
      const already = requested.has(s.login);
      const tags: string[] = [];
      if (already) tags.push("requested");
      tags.push("suggested");
      if (s.isAuthor) tags.push("author");
      if (s.isCommenter) tags.push("commenter");
      items.push({
        key: s.login,
        label: s.login,
        hint: `(${tags.join(", ")})`,
      });
      seen.add(s.login);
    }
    for (const login of row.pr.requestedReviewers) {
      if (seen.has(login)) continue;
      if (skipSelf(login)) continue;
      items.push({ key: login, label: login, hint: "(requested)" });
      seen.add(login);
    }
    for (const c of contributors) {
      if (seen.has(c.login)) continue;
      if (skipSelf(c.login)) continue;
      items.push({
        key: c.login,
        label: c.login,
        hint: `(${c.contributions} commits)`,
      });
      seen.add(c.login);
    }
    if (items.length === 0) {
      toast("no reviewer candidates", theme.warn, 2000);
      return;
    }
    setModal({
      kind: "reviewerPicker",
      title: `edit reviewers for #${row.pr.number}`,
      items,
      index: 0,
      checked: new Set(requested),
      original: new Set(requested),
      slug,
      prNumber: row.pr.number,
    });
  }

  async function submitReviewerPicker(): Promise<void> {
    if (modal?.kind !== "reviewerPicker") return;
    const { slug, prNumber, checked, original } = modal;
    const log = createLogger(slug);
    const branch = rows.find((r) => r.wt.slug === slug)?.wt.branch;
    setModal(null);
    if (!branch) {
      // Slug disappeared between picker open and submit (race against
      // a destroy). The mutation would still succeed at the gh layer,
      // but the optimistic patch has nothing to target — bail rather
      // than silently dropping the cache update.
      log.event.warn(`slug ${slug} no longer present; aborting reviewer edit`);
      toast("worktree gone, edit aborted", theme.warn, 2500);
      return;
    }
    const add: string[] = [];
    const remove: string[] = [];
    for (const k of checked) if (!original.has(k)) add.push(k);
    for (const k of original) if (!checked.has(k)) remove.push(k);
    if (add.length === 0 && remove.length === 0) {
      toast("no changes", theme.fgDim, 1500);
      return;
    }
    try {
      await mutate<GithubData>({
        filter: { queryKey: ["github"] },
        patch: (data) =>
          patchPullRequest(data, branch, (pr) => ({
            ...pr,
            requestedReviewers: [...checked],
            reviewRequests: pr.reviewRequests + add.length - remove.length,
          })),
        run: async () => {
          const result = await editReviewers(prNumber, { add, remove });
          if (!result.ok) throw new Error(result.error);
        },
      });
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      log.event.err(`edit reviewers failed for #${prNumber}: ${msg}`);
      toast(`edit reviewers failed: ${msg}`, theme.err, 4000);
      return;
    }
    const parts: string[] = [];
    if (add.length > 0) parts.push(`+${add.join(", ")}`);
    if (remove.length > 0) parts.push(`-${remove.join(", ")}`);
    log.event.ok(`edited reviewers for #${prNumber}: ${parts.join("; ")}`);
    const summary = [
      add.length > 0 ? `added ${add.length}` : null,
      remove.length > 0 ? `removed ${remove.length}` : null,
    ]
      .filter(Boolean)
      .join(", ");
    toast(summary, theme.ok, 2500);
  }

  // GitHub PR mutation flows — extracted to `flows/github-pr.ts`.
  // Rebuilt per render so the closures see fresh rows.
  const { doMarkReady, doAutoMerge, doShipPr, doTailFailedChecks } = makeGithubPrFlows({
    rows,
    toast,
    mutate,
  });

  function buildActionPickerItems(slug: string): PickerItem[] {
    const row = rows.find((r) => r.wt.slug === slug);
    const rowState = {
      pr: row?.pr,
      deployed: row?.fields.deploy.data ?? false,
    };
    const defs = [...config.actions, ...BUILTIN_ACTIONS];
    const keyById = assignActionKeys(defs);
    const actionItems = defs.map((def) => ({
      kind: "action" as const,
      def,
      key: keyById.get(def.id) ?? "",
      availability: evaluateActionRequirements(def.requires, rowState),
    }));
    // Cluster by group: group order by first appearance, original order
    // within a group, so the picker shows one header per section. Keys
    // are assigned over the unclustered list above so they stay stable
    // regardless of grouping. The custom-prompt entry always trails.
    const buckets = new Map<string, typeof actionItems>();
    for (const it of actionItems) {
      const g = it.def.group ?? "";
      const arr = buckets.get(g);
      if (arr) arr.push(it);
      else buckets.set(g, [it]);
    }
    return [...[...buckets.values()].flat(), { kind: "custom" as const }];
  }

  /**
   * Returns true if the item is launchable. For unavailable actions
   * toasts the reason so the user understands the no-op without
   * having to scan the dim subtitle in the picker. Used at both the
   * Enter and quick-pick-digit handlers so an unavailable action
   * can't slip into the edit modal.
   */
  function canPickAction(item: PickerItem): boolean {
    if (item.kind === "custom") return true;
    if (item.availability.ok) return true;
    toast(`${item.def.name}: ${item.availability.reason}`, theme.warn, 2500);
    return false;
  }

  function openActionPicker(slug: string): void {
    setModal({
      kind: "actionPicker",
      state: { mode: "list", slug, index: 0 },
    });
  }


  async function doNew(raw: string, defaultBase?: string): Promise<void> {
    const parsed = parseNewInput(raw, defaultBase);
    if ("error" in parsed) {
      newLog.event.err(parsed.error);
      return;
    }
    newLog.event.info(`resolving ${parsed.input}`);
    if (parsed.anyAuthor) newLog.event.info("searching all authors (--any)");
    if (parsed.base) newLog.event.info(`base: ${parsed.base}`);
    let branch: string;
    try {
      branch = await parseInput(parsed.input, {
        anyAuthor: parsed.anyAuthor,
        promptForChoice: (id, branches) =>
          new Promise<string | null>((resolve) => {
            setModal({
              kind: "branchPicker",
              title: `multiple branches for ${id}`,
              items: branches,
              index: 0,
              resolve,
            });
          }),
      });
    } catch (err) {
      newLog.event.err(err instanceof Error ? err.message : String(err));
      newLog.error(err instanceof Error ? err : String(err));
      return;
    }
    newLog.event.info(`branch = ${branch}`);
    const result = await createWorktree(branch, {
      onPhase: (p) => newLog.event.info(`phase: ${p}`),
      onLog: (line) => newLog.event.dim(line),
      runInstall: true,
      base: parsed.base,
    });
    if (!result.ok) {
      newLog.event.err(result.reason);
      return;
    }
    newLog.event.ok(`ready at ${result.path}`);
    void refreshAll();
  }

  // Check out a review-requested PR's branch as a worktree and drop it
  // into the "Reviews" section. The branch already exists on origin, so
  // `createWorktree` takes the checkout-existing path (sets upstream,
  // installs packages); `setSection` materializes the section by simply
  // assigning the new slug to it. Leaves the review-request row in place
  // — this spawns a worktree, it doesn't consume the PR.
  async function doCheckoutReview(branch: string): Promise<void> {
    const log = createLogger("[review]");
    log.event.info(`creating review worktree for ${branch}`);
    const result = await createWorktree(branch, {
      onPhase: (p) => log.event.info(`phase: ${p}`),
      onLog: (line) => log.event.dim(line),
      runInstall: true,
    });
    if (!result.ok) {
      log.event.err(result.reason);
      toast(`worktree failed: ${result.reason}`, theme.err, 3000);
      return;
    }
    await setSection(result.slug, REVIEW_SECTION);
    log.event.ok(`ready at ${result.path} → ${REVIEW_SECTION}`);
    toast(`created ${result.slug} in ${REVIEW_SECTION}`, theme.info, 2200);
    void refreshAll();
  }

  // Per-modal key handlers. Exactly one modal is active at a time;
  // `useKeyboard` below dispatches on `modal.kind` and each handler
  // owns its modal's full key map, swallowing the keypress.

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
    // rename-section prompt — `purpose` discriminates which.
    if (footer.kind === "input") {
      if (k.name === "escape" || (k.ctrl && k.name === "c")) {
        setFooter({ kind: "legend" });
        setPendingRename(null);
        return;
      }
      if (k.name === "return") {
        const raw = footer.value.trim();
        const base = footer.base;
        const purpose = footer.purpose;
        setFooter({ kind: "legend" });
        if (purpose === "rename-section") {
          const oldName = pendingRename;
          setPendingRename(null);
          if (!oldName || !raw || raw === oldName) return;
          renameSection(oldName, raw).catch((err) => {
            const msg = err instanceof Error ? err.message : String(err);
            appLog.event.err(`rename failed: ${msg}`);
            toast(`rename failed: ${msg}`, theme.err, 3000);
          });
          // Update sticky last-move-target so a stale name doesn't
          // dangle as the picker default.
          setLastMoveTarget((prev) => (prev === oldName ? raw : prev));
          toast(`renamed "${oldName}" to "${raw}"`, theme.info, 1800);
          return;
        }
        if (raw) void doNew(raw, base);
        return;
      }
      if (k.name === "backspace") {
        // Backspace on empty input exits, matching the filter convention.
        if (footer.value.length === 0) {
          setFooter({ kind: "legend" });
          return;
        }
        setFooter({ ...footer, value: footer.value.slice(0, -1) });
        return;
      }
      // `k.sequence` is the literal bytes the terminal delivered — a
      // single key for typing, or a paste blob. Filter to printable
      // ASCII so control chars in the middle of a paste don't corrupt.
      const text = printableText(k.sequence);
      if (text) setFooter({ ...footer, value: footer.value + text });
      return;
    }

    // Normal mode.
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
    // Shift+A — pause / resume ALL automations for this session. The
    // pending intent queue is dropped on pause; conditions that still
    // hold re-derive it on resume.
    if (isShiftedLetter(k, "a")) {
      if (!automations.configured) {
        toast("no [[automations]] configured", theme.fgDim, 2000);
        return;
      }
      const nowPaused = automations.togglePaused();
      toast(
        nowPaused ? "automations paused" : "automations resumed",
        nowPaused ? theme.warn : theme.ok,
        2000,
      );
      return;
    }
    // Ctrl+A — toggle automations for the CURRENT worktree (persisted
    // in wtstate, survives restarts). The per-wt escape hatch when a
    // branch is under manual surgery.
    if (k.ctrl && k.name === "a" && !k.shift && !k.option && !k.meta) {
      if (!automations.configured) {
        toast("no [[automations]] configured", theme.fgDim, 2000);
        return;
      }
      if (!current) {
        toast("select a worktree first", theme.warn, 1500);
        return;
      }
      const slug = current.wt.slug;
      void (async () => {
        try {
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
    if (isPlainLetter(k, "q") || (k.ctrl && k.name === "c")) {
      quit();
      return;
    }
    if (k.sequence === "?") {
      setModal({ kind: "help", query: "", searching: false });
      return;
    }
    if (k.sequence === "r") {
      appLog.event.dim("refresh");
      void refreshAll();
      return;
    }
    // `R` — restack the stack the selected worktree belongs to (whole stack,
    // algorithmic; escalates to /restack only on a conflict bail).
    if (k.sequence === "R") {
      void doReplayStack();
      return;
    }
    // Ctrl+R: clear all caches. Moved off bare R when R lost its
    // single-letter slot; same confirm flow, same handler.
    if (k.ctrl && k.name === "r") {
      setModal({
        kind: "confirm",
        pendingKey: "R",
        title: "clear caches",
        message: "Clear all cached data and refetch from scratch?",
        confirmLabel: "clear",
      });
      return;
    }
    if (k.sequence === "n") {
      newLog.event.dim("tip: --any to match any author, --base <ref> to branch off");
      setFooter({ kind: "input", prompt: "new:", value: "", purpose: "new" });
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
    if (isPlainLetter(k, "c")) {
      if (cleanCandidates.length === 0) {
        toast("nothing to clean", theme.fgDim, 1500);
        return;
      }
      setModal({ kind: "cleanConfirm" });
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
    // Shift+TAB — cycle the primary harness selection. Re-rendered top-
    // right indicator reflects the new primary; subsequent F12 spawns
    // pick it up.
    if (
      k.name === "tab" &&
      k.shift &&
      !k.ctrl &&
      !k.option &&
      !k.super &&
      !k.hyper &&
      !k.meta
    ) {
      void (async () => {
        const next = await cyclePrimaryHarness();
        appLog.event.info(`primary harness → ${getHarness(next).label}`);
      })();
      return;
    }
    // Toggle into a persistent harness session for a session slot —
    // `,` is the wt source repo (config/self edits), `.` is the
    // configured main clone. Same model as F12 on a worktree row:
    // tmux's `new-session -A` makes re-entry idempotent, and F12
    // (bound to detach-client in the wt-private tmux config) takes
    // the user back out. The selected primary harness (TAB to cycle)
    // is the spawned kind, mirroring how row F12 picks a harness.
    if (k.sequence === ",") {
      doEnterSlotSession(WT_SOURCE_SLOT);
      return;
    }
    if (k.sequence === ".") {
      doEnterSlotSession(MAIN_CLONE_SLOT);
      return;
    }
    if (k.sequence === "/") {
      doEnterSlotSession(DOTFILES_SLOT);
      return;
    }
    if (k.sequence === ">") {
      openInZed(WT_SOURCE_SLOT.path);
      wtSourceLog.event.info(`opened ${WT_SOURCE_SLOT.path}`);
      return;
    }
    if (k.sequence === "O") {
      openInZed(MAIN_CLONE_SLOT.path);
      createLogger(MAIN_CLONE_SLOT.label).event.info(
        `opened ${MAIN_CLONE_SLOT.path}`,
      );
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
      const dirty = current.fields.dirty.data?.length ?? 0;
      const ahead = current.fields.sync.data?.remote?.ahead ?? 0;
      const reasons: string[] = [];
      if (dirty > 0) {
        reasons.push(`${dirty} uncommitted file${dirty === 1 ? "" : "s"}`);
      }
      if (ahead > 0) {
        reasons.push(`${ahead} unpushed commit${ahead === 1 ? "" : "s"}`);
      }
      if (reasons.length > 0) {
        setModal({
          kind: "confirm",
          pendingKey: "d!",
          title: "force remove",
          message: `Force remove ${current.wt.slug}?`,
          detail: `${reasons.join(", ")} will be lost.`,
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
        <Details
          row={current}
          reviewRequest={selectedPr}
          section={sectionDetail}
          width={Math.max(0, width - listWidth)}
          scrollRef={detailsScrollRef}
        />
      </box>
      <OutputViewer output={displayedOutput} height={activityHeight} />
      {modal?.kind === "outputsPicker" ? (
        <OutputsPicker
          slug={currentSlug ?? null}
          items={visibleOutputs}
          selectedIndex={
            visibleOutputs.length === 0
              ? 0
              : Math.min(
                  Math.max(0, modal.index),
                  visibleOutputs.length - 1,
                )
          }
        />
      ) : null}
      {modal?.kind === "claudeSessionsPicker" ? (
        <SessionsPickerList
          slug={modal.slug}
          rows={pickerRows}
          selectedIndex={Math.min(
            Math.max(0, modal.index),
            Math.max(0, pickerRows.length - 1),
          )}
          summaries={pickerSummaries}
        />
      ) : null}
      {modal?.kind === "claudeSessionsNew" ? (
        <SessionsPickerNew
          slug={modal.slug}
          input={modal.input}
          autoName={nextAutoName(modal.slug)}
          error={modal.error}
        />
      ) : null}
      {modal?.kind === "argPicker" ? (
        <ArgPickerModal
          title={modal.def.name}
          prompt={modal.def.argPrompt?.label ?? ""}
          history={modal.history}
          index={Math.min(
            Math.max(0, modal.index),
            modal.history.length, // trailing "+ new"
          )}
          input={modal.input}
        />
      ) : null}
      {modal?.kind === "harnessSelect" ? (
        <HarnessPickerModal
          slug={modal.slug}
          selectedIndex={Math.min(
            Math.max(0, modal.index),
            HARNESSES.length - 1,
          )}
        />
      ) : null}
      <Footer mode={footer} hint={footerHint} />
      {modal?.kind === "help" ? (
        <HelpOverlay query={modal.query} searching={modal.searching} />
      ) : null}
      {modal?.kind === "cleanConfirm" ? (
        <CleanConfirmModal candidates={cleanCandidates} />
      ) : null}
      {modal?.kind === "yank" && current ? <YankModal row={current} /> : null}
      {modal?.kind === "branchPicker" ? (
        <PickerModal
          title={modal.title}
          items={modal.items}
          selectedIndex={modal.index}
        />
      ) : null}
      {modal?.kind === "basePicker" ? (
        <PickerModal
          title={`fork base for ${modal.slug}`}
          items={modal.items.map((it) => it.label)}
          selectedIndex={modal.index}
          toggleKey="b"
        />
      ) : null}
      {modal?.kind === "reviewerPicker" ? (
        <MultiPickerModal
          title={modal.title}
          items={modal.items}
          selectedIndex={modal.index}
          checked={modal.checked}
          toggleKey="v"
        />
      ) : null}
      {modal?.kind === "sectionPicker" ? (
        <SectionPickerModal
          title={modal.title}
          items={modal.items}
          selectedIndex={modal.index}
          newName={modal.newName}
        />
      ) : null}
      {modal?.kind === "actionPicker" && modal.state.mode === "list" ? (
        <ActionPickerModal
          slug={modal.state.slug}
          items={buildActionPickerItems(modal.state.slug)}
          selectedIndex={modal.state.index}
        />
      ) : null}
      {modal?.kind === "actionPicker" && modal.state.mode === "edit" ? (
        <ActionEditModal
          slug={modal.state.slug}
          def={modal.state.def}
          extras={modal.state.extras}
          vars={(() => {
            const row = rows.find((r) => r.wt.slug === modal.state.slug);
            return row
              ? buildActionVars(
                  row,
                  actionSkillPrefix(modal.state.def, primaryHarness),
                )
              : {};
          })()}
        />
      ) : null}
      {modal?.kind === "killActionConfirm" ? (
        <KillActionConfirmModal
          slug={modal.slug}
          actionName={modal.actionName}
        />
      ) : null}
      {modal?.kind === "killSessionConfirm" ? (
        <KillSessionConfirmModal slug={modal.slug} sessionKind={modal.sessionKind} />
      ) : null}
      {modal?.kind === "confirm" ? (
        <ConfirmModal
          title={modal.title}
          message={modal.message}
          detail={modal.detail}
          confirmLabel={modal.confirmLabel}
          danger={modal.danger}
        />
      ) : null}
    </box>
  );
}
