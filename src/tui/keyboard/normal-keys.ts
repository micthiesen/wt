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
import { Data, Effect } from "effect";

import { actionRegistry } from "../../core/actions.ts";
import { emptyEdit } from "../text-edit.tsx";
import type { WorkState } from "../../core/work-status.ts";
import { config, type PullRequestTarget } from "../../core/config.ts";
import { getHarness, VISIBLE_HARNESSES, type HarnessId } from "../../core/harness/index.ts";
import {
  issueUrlForId,
  preferredIssueUrl,
} from "../../core/issue-tracker.ts";
import { lockLabel, lockStatus } from "../../core/locks.ts";
import { createLogger } from "../../core/logger.ts";
import { eventsOutputId, firehoseOutputId, indexOfOutput } from "../../core/outputs.ts";
import { stageUrl } from "../../core/stage.ts";
import { StatusKind } from "../../core/types.ts";
import {
  worktreeActionKey,
} from "../../core/worktree-target.ts";
import { setAttentionSeen } from "../../core/wtstate.ts";
import {
  destroyHazardLabel,
  destroyHazards,
  isBareKey,
  isBareShiftedKey,
  isPlainLetter,
  isShiftedLetter,
  sectionJumpTarget,
  sessionLaunchBlockedReason,
} from "../app-helpers.ts";
import { events as activityEvents } from "../activity-log.ts";
import { verifyStepsOpenByDefault } from "../panels/details/work-status-block.tsx";
import { activityScroll } from "../panels/activity.tsx";
import { SCROLL_STEP } from "../scrollbox.tsx";
import { firstYankIndex, sectionYankItems, yankItemsFor } from "../panels/yank.tsx";
import type { HarnessRoute } from "../sessions/worktree.ts";
import type { makeBaseFlows } from "../flows/base.ts";
import type { makeDestroyFlows } from "../flows/destroy.ts";
import type { makeGithubPrFlows } from "../flows/github-pr.ts";
import { REVIEW_SECTION } from "../flows/new-worktree.ts";
import type { makeSectionFlows } from "../flows/sections.ts";
import type { makeSessionFlows } from "../flows/sessions.ts";
import { openUrlHidingTerminal } from "../../core/macos.ts";
import { openInEditor } from "../../core/editor.ts";
import { forkReported } from "../effect-boundary.ts";
import {
  isSyntheticLiveSessionId,
  type useHarnessSessions,
} from "../hooks/useHarnessSessions.ts";
import type { useOutputFocus } from "../hooks/useOutputFocus.ts";
import { GROUP_ARCHIVED, GROUP_INBOX } from "../hooks/useWorktreeRows.ts";
import { visualKey, type useVisualItems } from "../hooks/useVisualItems.ts";
import type { Modal } from "../modal-state.ts";
import type { FooterMode } from "../panels/footer.tsx";
import type { ListScrollHandle } from "../panels/list.tsx";
import { isRemoteSummary, remoteEntryKey } from "../remote-creation.ts";
import { theme } from "../theme.ts";
import type { WorktreeModel } from "../worktree-model.ts";

const appLog = createLogger("[app]");
const newLog = createLogger("[new]");

class NormalKeyActionError extends Data.TaggedError("NormalKeyActionError")<{
  readonly label: string;
  readonly cause: unknown;
}> {}

const keyPromise = <A>(label: string, evaluate: () => Promise<A>) =>
  Effect.tryPromise({
    try: evaluate,
    catch: (cause) => new NormalKeyActionError({ label, cause }),
  });

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
  selectedRemote: VisualItems["selectedRemote"];
  selectedWorktree: VisualItems["selectedWorktree"];
  selectedSection: VisualItems["selectedSection"];
  currentTarget: VisualItems["currentTarget"];
  visualItems: VisualItems["visualItems"];
  cursorIndex: VisualItems["cursorIndex"];
  currentSlug: string | undefined;
  setSel: (key: string | null) => void;
  /** Re-aim the cursor off rows that are leaving their slot — see
   *  `cursorSuccessor`. */
  advanceCursorPast: (keys: readonly string[]) => void;
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
  doEnterWorktreeSession: (
    worktree: WorktreeModel,
    target: "shell" | "diff" | "harness",
    harnessRoute?: HarnessRoute,
  ) => void;
  doEnterHarnessSession: ReturnType<typeof makeSessionFlows>["doEnterHarnessSession"];
  // Flows
  handleGlobalKey: (k: KeyEvent) => boolean;
  doShiftMove: ReturnType<typeof makeSectionFlows>["doShiftMove"];
  openSectionPicker: ReturnType<typeof makeSectionFlows>["openSectionPicker"];
  openSectionRename: ReturnType<typeof makeSectionFlows>["openSectionRename"];
  openIssueIdPrompt: () => void;
  openBasePicker: ReturnType<typeof makeBaseFlows>["openBasePicker"];
  openStatusPicker: () => void;
  openActionPicker: (target: NonNullable<VisualItems["currentTarget"]>) => void;
  openReviewerPicker: (slug: string) => Promise<void>;
  doReplayStack: ReturnType<typeof makeDestroyFlows>["doReplayStack"];
  doTailFailedChecks: ReturnType<typeof makeGithubPrFlows>["doTailFailedChecks"];
  // Automations
  automations: { configured: boolean };
  toggleAutomationsPaused: (slug: string) => Promise<boolean>;
  toggleStackAutomationsPaused: (stackId: string, memberSlugs: readonly string[]) => Promise<boolean>;
  // Actions on the row
  toggleArchived: (key: string) => Promise<{ archived: boolean }>;
  dismissReviewRequest: (url: string, updatedAt: string) => Promise<void>;
  setWorktreeSection: (
    target: WorktreeModel["target"],
    section: string | null,
  ) => Promise<void>;
  toggleSectionFold: (key: string) => Promise<boolean>;
  setSectionFolded: (key: string, folded: boolean) => Promise<boolean>;
  refreshAiSummary: (slug: string) => Promise<boolean>;
  /** `V`'s override of the details pane's post-merge-steps block. */
  verifyExpanded: boolean | null;
  setVerifyExpanded: (v: boolean | null) => void;
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
    selectedRemote,
    selectedWorktree,
    selectedSection,
    currentTarget,
    visualItems,
    cursorIndex,
    currentSlug,
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
    handleGlobalKey,
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
    verifyExpanded,
    setVerifyExpanded,
    toast,
    reportActionError,
  } = ctx;

  const runAction = <A>(
    label: string,
    evaluate: () => Promise<A>,
  ): void => {
    forkReported(keyPromise(label, evaluate), (error) =>
      reportActionError(error.label, error.cause),
    );
  };

  const f12Route = (): HarnessRoute => {
    const target = currentHarnessSessions.f12Target;
    if (!target) return { harnessId: primaryHarness };
    const isSyntheticLive = isSyntheticLiveSessionId(target.sessionId);
    const resumeSessionId = target.isLive || isSyntheticLive ? null : target.sessionId;
    return {
      harnessId: target.harnessId,
      managedName: target.extras.managedName,
      resumeSessionId,
      freshSlot: getHarness(target.harnessId).singleSlot && resumeSessionId !== null,
    };
  };
    // Drop Esc-prefixed keys outright. Outside the kitty protocol a
    // terminal encodes Alt+<key> as the two bytes `\x1b` `<key>`, which
    // is byte-identical to Esc followed by that key — so the parser
    // reports meta on a bare `j` typed quickly after dismissing a modal,
    // and on anything a terminal binding emits Esc-prefixed (a Cmd layer
    // sending `\x1bu`, say). wt binds NOTHING to Alt/Meta in normal
    // mode, so the only thing such an event can do is impersonate the
    // unmodified key and fire a random binding — scroll a pane, open the
    // status picker. Swallowing is the honest reading of an
    // unrepresentable chord: an unsupported modifier does nothing.
    // Narrow on the two-byte form so a paste blob that happens to begin
    // with Esc isn't caught; bare Esc parses with meta false (name
    // "escape") and reaches the handler below untouched.
    if (k.meta && k.sequence.length === 2) return;
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
      const pickerBlocked = sessionLaunchBlockedReason(current);
      if (pickerBlocked) {
        toast(`${current.wt.slug} is ${pickerBlocked}`, theme.warn, 2000);
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
    // `"` jumps to the attention feed; pressing it again while the
    // attention feed is showing cycles to the unfiltered firehose (and
    // back) — curated by default, everything one keystroke away.
    if (k.sequence === '"') {
      const next =
        displayedOutput.id === eventsOutputId()
          ? firehoseOutputId()
          : eventsOutputId();
      setFocus(currentSlug ?? null, { focused: next });
      return;
    }
    // `x` — mark the attention feed seen, only while it's the displayed
    // pane (a blind global key would invite accidental marks). Records
    // a watermark: everything at or before it renders dim below a
    // `── seen` rule, so the feed shows only new stuff after you've
    // worked through it. Persisted (wtstate) so the boot backfill
    // re-seeds handled lines dimmed. Display-only — nothing is
    // deleted, and the firehose/daily log are untouched.
    if (k.sequence === "x" && displayedOutput.id === eventsOutputId()) {
      if (activityEvents.getAttentionSnapshot().length === 0) {
        toast("attention feed is empty", theme.fgDim, 1500);
        return;
      }
      const ts = Date.now();
      activityEvents.markSeen(ts);
      try {
        setAttentionSeen(ts);
      } catch (err) {
        reportActionError("mark seen", err);
        return;
      }
      // Snap to the live tail: "marked seen" means "show me only new
      // stuff", which is at the bottom — and the jump re-engages
      // sticky-bottom if the user had scrolled back.
      activityScroll.current?.scrollBy(9999, "viewport");
      toast("attention marked seen", theme.fgDim, 1500);
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
    // `#` attaches (or clears) the row's tracker id. A sequence key
    // rather than a letter on purpose: the editing family is out of
    // free letters, `i`/`I` already mean "open the issue", and `#`
    // reads as an id everywhere else it appears.
    if (k.sequence === "#") {
      openIssueIdPrompt();
      return;
    }
    // App-level keys shared with the removed-worktrees view (help,
    // quit, refresh, ^R, n, c, Shift+A, Shift+Tab, slot sessions, editor).
    if (handleGlobalKey(k)) return;
    // Ctrl+A — toggle automations for the row under the cursor
    // (persisted in wtstate, survives restarts). A stack member
    // pauses/resumes the WHOLE stack as one, keyed by stackId (the root
    // branch) so members stacked on later stay covered; a non-stack row
    // toggles just itself. The escape hatch when a branch (or stack) is
    // under manual surgery.
    if (k.ctrl && k.name === "a" && !k.shift && !k.option && !k.meta) {
      if (!automations.configured) {
        toast("no [[automations]] configured", theme.fgDim, 2000);
        return;
      }
      const stackId = current?.stack?.stackId ?? null;
      if (!stackId && !current) {
        toast("select a worktree first", theme.warn, 1500);
        return;
      }
      forkReported(
        Effect.gen(function* () {
          if (stackId) {
            const memberSlugs = visualItems.flatMap((v) =>
              v.kind === "wt" && v.row.stack?.stackId === stackId
                ? [v.row.wt.slug]
                : [],
            );
            const nowPaused = yield* keyPromise(
              "automations toggle",
              () => toggleStackAutomationsPaused(stackId, memberSlugs),
            );
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
          const nowPaused = yield* keyPromise(
            "automations toggle",
            () => toggleAutomationsPaused(slug),
          );
          createLogger(slug).event.info(
            nowPaused ? "automations paused for this worktree" : "automations resumed for this worktree",
          );
          toast(
            nowPaused ? `automations paused for ${slug}` : `automations resumed for ${slug}`,
            nowPaused ? theme.warn : theme.ok,
            2000,
          );
        }),
        (error) => reportActionError(error.label, error.cause),
      );
      return;
    }
    // Ctrl+Shift+J / Ctrl+Shift+K scroll the BOTTOM pane's event feed.
    // Checked BEFORE the plain-Ctrl details scroll below, which would
    // otherwise swallow the chord. Kitty csi-u delivers the base letter
    // plus a shift flag (never the uppercase literal); legacy encodings
    // can't express Ctrl+Shift+letter at all — there the chord collapses
    // to plain Ctrl+J/K and scrolls the details pane instead, with
    // Ctrl+E/Y below still reaching the feed. Known degradation,
    // accepted: every kitty-protocol terminal gets it right.
    if (k.ctrl && k.shift && (k.name === "j" || k.name === "k")) {
      activityScroll.current?.scrollBy(
        k.name === "j" ? SCROLL_STEP : -SCROLL_STEP,
      );
      return;
    }
    // Ctrl+J / Ctrl+K scroll the DETAILS pane (worktree or review
    // request) by the standard SCROLL_STEP rows — the pane read
    // alongside the cursor row earns the prime j/k chord; the feed took
    // Ctrl+Shift above and keeps the Ctrl+E / Ctrl+Y (vim scroll)
    // alias. List navigation stays on bare j/k. Ctrl+J
    // arrives as "linefeed" in legacy terminals (it's the LF byte,
    // special-cased ahead of ctrl-letter mapping) and as ctrl+"j" under
    // the kitty keyboard protocol — accept both. No-op when the content
    // fits; key-repeat covers long panes. Mouse wheel works natively
    // over either pane.
    if (k.name === "linefeed" || (k.ctrl && (k.name === "j" || k.name === "k"))) {
      detailsScrollRef.current?.scrollBy(
        k.name === "k" ? -SCROLL_STEP : SCROLL_STEP,
      );
      return;
    }
    // Feed alias: vim's Ctrl+E / Ctrl+Y. Sticky-bottom releases while
    // scrolled back and re-engages at the bottom edge.
    //
    // There is deliberately NO Alt+J / Alt+K alias here. Outside the
    // kitty protocol, Alt+<letter> is indistinguishable from Esc
    // followed by that letter — the bytes are identical (`\x1b` `j`),
    // and the parser reports meta whenever a read chunk starts with
    // Esc. So a plain `j` typed quickly after dismissing a modal
    // arrives as meta+j, as does any terminal binding that emits an
    // Esc-prefixed letter. Binding meta+j/k therefore hijacks bare
    // list navigation at random, which is strictly worse than the
    // alias is useful: the terminals where it would be unambiguous
    // (kitty protocol) are exactly the ones where Ctrl+Shift+J/K
    // already works, and legacy terminals keep Ctrl+E/Y.
    if (k.ctrl && (k.name === "e" || k.name === "y")) {
      activityScroll.current?.scrollBy(
        k.name === "e" ? SCROLL_STEP : -SCROLL_STEP,
      );
      return;
    }
    // Ctrl+D / Ctrl+U jump between section starts. An expanded section's
    // first row is its cursor stop; a folded section is represented by its
    // selectable title, so the same helper lands where TAB can unfold it.
    if (
      k.ctrl &&
      !k.shift &&
      !k.option &&
      !k.meta &&
      (k.name === "d" || k.name === "u")
    ) {
      const target = sectionJumpTarget(
        visualItems,
        cursorIndex,
        k.name === "d" ? 1 : -1,
      );
      if (target !== null) setSel(target);
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
    // `Space` — jump to the next row needing attention (needs-human /
    // needs-testing / ready), scanning forward from the cursor and
    // wrapping. Status sort ranks urgency only WITHIN each group, so on
    // a multi-section fleet this is the cross-section scan the sort
    // can't express. Record states only (the asserted queue); rows
    // hidden inside folded sections aren't in `visualItems` and are
    // skipped by construction.
    if (k.name === "space" || k.sequence === " ") {
      const urgent = new Set<WorkState>(["needs-human", "needs-testing", "ready"]);
      const n = visualItems.length;
      for (let step = 1; step <= n; step++) {
        const it = visualItems[(Math.max(0, cursorIndex) + step) % n];
        if (
          it?.kind === "wt" &&
          it.row.work &&
          urgent.has(it.row.work.state)
        ) {
          setSel(visualKey(it));
          return;
        }
      }
      toast("nothing needs you", theme.fgDim, 1500);
      return;
    }
    // `R` — rebase/restack whatever's selected: a stack member restacks
    // the whole stack, a standalone worktree rebases onto its recorded
    // base or trunk (algorithmic; escalates to /restack on a conflict bail).
    if (k.sequence === "R") {
      runAction("stack replay", doReplayStack);
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
        edit: emptyEdit,
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
      if (!currentTarget) {
        toast("select a worktree first", theme.warn, 1500);
        return;
      }
      const actionKey = worktreeActionKey(currentTarget);
      const run = actionRegistry.get(actionKey);
      if (run?.status === "running") {
        setModal({
          kind: "killActionConfirm",
          slug: actionKey,
          actionName: run.actionName,
        });
      } else {
        openActionPicker(currentTarget);
      }
      return;
    }
    // F10 — toggle into the selected worktree's plain shell session.
    // Persistent like F12: detach with F10, reattach to find
    // scrollback, env, and any background processes still alive.
    // `exit` / Ctrl+D ends the session.
    if (isBareKey(k, "f10")) {
      if (!selectedWorktree) {
        toast("select a worktree first", theme.warn, 1500);
        return;
      }
      doEnterWorktreeSession(selectedWorktree, "shell", f12Route());
      return;
    }
    // F11 — toggle into the selected worktree's diff TUI
    // (`[diff].command`, default `revdiff`). tmux's `new-session -A`
    // makes this idempotent (creates or attaches), and the
    // wt-private tmux config binds F11 to detach-client → the same
    // physical key flips between contexts. Sessions persist (named
    // `<slug>-diff`) so detach-then-reattach keeps the diff TUI's scroll +
    // expansion state. Init locks are allowed once the checkout exists;
    // destructive operations remain blocked so we don't race a destroy.
    if (isBareKey(k, "f11")) {
      if (!selectedWorktree) {
        toast("select a worktree first", theme.warn, 1500);
        return;
      }
      doEnterWorktreeSession(selectedWorktree, "diff", f12Route());
      return;
    }
    // Shift+F10 — kill-confirm for the selected worktree's shell
    // session. Mirrors Shift+F11/F12. No-op (with a hint) when
    // there's no session. Killing terminates any background
    // processes the user launched in the shell.
    if (isBareShiftedKey(k, "f10")) {
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
    // session. Killing throws away the diff TUI's scroll/expansion state, so
    // next F11 opens fresh.
    if (isBareShiftedKey(k, "f11")) {
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
    if (isBareShiftedKey(k, "f12")) {
      if (selectedWorktree?.source.kind === "remote") {
        doEnterWorktreeSession(selectedWorktree, "harness");
        return;
      }
      if (!current) {
        toast("select a worktree first", theme.warn, 1500);
        return;
      }
      const slug = current.wt.slug;
      const blocked = sessionLaunchBlockedReason(current);
      if (blocked) {
        toast(`${slug} is ${blocked}`, theme.warn, 2000);
        return;
      }
      // Default highlight = current primary, so the muscle-memory
      // "Shift+F12, F12 again" path spawns whatever TAB selected.
      const initialIdx = VISIBLE_HARNESSES.findIndex((h) => h.id === primaryHarness);
      setModal({
        kind: "harnessSelect",
        slug,
        index: initialIdx >= 0 ? initialIdx : 0,
      });
      return;
    }
    // F12 — toggle into the selected worktree's "F12 target" harness
    // session. Target = a live session in the selected primary harness,
    // otherwise another live harness, otherwise the selected harness's
    // durable primary conversation; if none exists, spawn fresh. tmux's
    // `new-session -A` makes attach-or-create idempotent. From inside
    // the session, the wt-private tmux config binds F12 to
    // detach-client so the same physical key flips between contexts.
    // Init locks are allowed once the checkout exists; destructive operations
    // remain blocked so we don't race a destroy.
    if (isBareKey(k, "f12")) {
      if (selectedWorktree?.source.kind === "remote") {
        doEnterWorktreeSession(selectedWorktree, "harness");
        return;
      }
      if (!current) {
        toast("select a worktree first", theme.warn, 1500);
        return;
      }
      const slug = current.wt.slug;
      const blocked = sessionLaunchBlockedReason(current);
      if (blocked) {
        toast(`${slug} is ${blocked}`, theme.warn, 2000);
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
    if (isBareKey(k, "tab")) {
      // Land the cursor sensibly across the async reflow: unfolding → the
      // section's first row; folding → the new header line.
      const item = currentItem;
      if (item?.kind === "section") {
        const first = item.members[0];
        setSel(
          first
            ? first.kind === "wt"
              ? first.row.wt.slug
              : `remote:${remoteEntryKey(first.entry)}`
            : `section:${item.sectionKey}`,
        );
        runAction("section fold", () => toggleSectionFold(item.sectionKey));
        return;
      }
      if (item?.kind === "wt") {
        // Inbox rows fold too — under the sentinel key, mirroring the
        // activeItems builder — and so does the archived block, which is
        // the one group nobody chose to have and everybody accumulates.
        const key = item.row.archived ? GROUP_ARCHIVED : item.row.section ?? GROUP_INBOX;
        setSel(`section:${key}`);
        runAction("section fold", () => toggleSectionFold(key));
        return;
      }
      if (item?.kind === "remote") {
        const key = item.archived
          ? GROUP_ARCHIVED
          : isRemoteSummary(item.entry)
            ? item.entry.section ?? GROUP_INBOX
            : GROUP_INBOX;
        setSel(`section:${key}`);
        runAction("section fold", () => toggleSectionFold(key));
        return;
      }
      toast("no section here to fold", theme.fgDim, 1500);
      return;
    }
    // `y` on a folded section header yanks the BATCH — its name, its
    // member slugs — which is the vocabulary the manager conversation
    // uses. The row form lives further down, guarded on `current`.
    if (selectedSection && k.sequence === "y") {
      setModal({ kind: "yank", index: firstYankIndex(sectionYankItems(selectedSection)) });
      return;
    }
    // Location-neutral row commands. PR and issue identity are part of the
    // shared worktree model; opening either never depends on where the checkout
    // lives.
    if (selectedWorktree) {
      const modelLog = createLogger(
        selectedWorktree.source.kind === "remote"
          ? `[remote:${selectedWorktree.source.row.hostLabel}]`
          : selectedWorktree.slug,
      );
      if (isPlainLetter(k, "p")) {
        if (!selectedWorktree.pr) {
          modelLog.event.warn("no PR for this branch");
          return;
        }
        openPrUrl(
          selectedWorktree.pr.url,
          selectedWorktree.pr.number,
          null,
          selectedWorktree.slug,
        );
        return;
      }
      if (isPlainLetter(k, "i")) {
        const url = preferredIssueUrl(
          selectedWorktree.slug,
          selectedWorktree.githubIssue,
          selectedWorktree.issueId,
        );
        if (!url) {
          modelLog.event.warn(
            "no issue URL (set a tracker id with `#`, or attach a --gh issue)",
          );
          return;
        }
        forkReported(openUrlHidingTerminal(url), (error) =>
          modelLog.debug(`open issue url failed: ${error.message}`),
        );
        modelLog.event.info("opened issue");
        return;
      }
      if (k.sequence === "I") {
        const url = issueUrlForId(selectedWorktree.issueId);
        if (!url) {
          modelLog.event.warn(
            "no primary issue URL (set a tracker id with `#`; non-GH ids also need [issue_tracker] url_template)",
          );
          return;
        }
        forkReported(openUrlHidingTerminal(url), (error) =>
          modelLog.debug(`open issue url failed: ${error.message}`),
        );
        modelLog.event.info("opened issue");
        return;
      }
      if (isPlainLetter(k, "a")) {
        if (selectedWorktree.source.kind === "local") {
          const archiveLock = lockStatus(selectedWorktree.slug);
          if (archiveLock) {
            toast(
              `${selectedWorktree.slug} is ${lockLabel(archiveLock)} — can't change archive state`,
              theme.warn,
              2500,
            );
            return;
          }
        }
        if (!selectedWorktree.archived && currentItem) {
          advanceCursorPast([visualKey(currentItem)]);
        }
        forkReported(
          Effect.gen(function* () {
            if (selectedWorktree.archived) {
              yield* keyPromise(
                "archive",
                () => setWorktreeSection(selectedWorktree.target, null),
              );
            }
            const { archived } = yield* keyPromise(
              "archive",
              () => toggleArchived(selectedWorktree.key),
            );
            if (archived) {
              yield* keyPromise(
                "archive",
                () => setSectionFolded(GROUP_ARCHIVED, true),
              );
            }
            if (!archived && currentItem) setSel(visualKey(currentItem));
            modelLog.event.info(
              archived ? "archived" : "restored to Inbox",
            );
            toast(
              archived
                ? `archived ${selectedWorktree.slug}`
                : `restored ${selectedWorktree.slug}`,
              theme.info,
              2000,
            );
          }),
          (error) => reportActionError(error.label, error.cause),
        );
        return;
      }
      if (isPlainLetter(k, "l")) {
        rememberPrTargetChord("linear");
        openSectionPicker();
        return;
      }
    }
    // Remote rows use the shared navigation keys and the F10/F11/F12 session
    // routes above. Deletion is forwarded through the remote host's normal
    // `wt rm` command so its lock, dirty, and unpushed-work checks stay
    // authoritative.
    if (selectedRemote) {
      if (isPlainLetter(k, "d")) {
        if (!isRemoteSummary(selectedRemote)) {
          toast("remote worktree is still being created", theme.warn, 1800);
          return;
        }
        // Do not gate a mutation on the inventory query's error state or its
        // cached busy flag. The host may have recovered since that read, and
        // remote `wt rm` performs the authoritative live lock check with a
        // bounded SSH timeout. A real refusal is surfaced by doRemoteRemove.
        const force = selectedRemote.dirty || (selectedRemote.unpushed ?? 0) > 0;
        const forceDetail = selectedRemote.dirty
          ? "Uncommitted changes will be lost. The remote branch will also be deleted."
          : `${selectedRemote.unpushed} unpushed commit${selectedRemote.unpushed === 1 ? "" : "s"} will be lost. The remote branch will also be deleted.`;
        setModal({
          kind: "confirm",
          pendingKey: force ? "d!" : "d",
          target: selectedWorktree?.target,
          title: force ? "force remove remote worktree" : "remove remote worktree",
          message: `Remove ${selectedRemote.slug} from ${selectedRemote.hostLabel}?`,
          detail: force
            ? forceDetail
            : "The remote branch will also be deleted.",
          confirmLabel: "remove",
          danger: true,
        });
      }
      return;
    }
    // Review-request rows: a tiny set of PR-only keybinds, no
    // worktree-keyed actions. Unmapped keys fall through to the wt
    // per-row block below, which is gated on `current` (undefined for
    // a PR selection) and silently no-ops.
    if (selectedPr) {
      const prLog = createLogger("[review]");
      if (isPlainLetter(k, "d")) {
        if (currentItem) advanceCursorPast([visualKey(currentItem)]);
        runAction("dismiss review request", async () => {
          await dismissReviewRequest(selectedPr.url, selectedPr.updatedAt);
          toast("review request dismissed", theme.fgDim, 1500);
        });
        return;
      }
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
      forkReported(
        openInEditor(current.wt.path).pipe(
          Effect.tap(() => Effect.sync(() => rowLog.event.info("opened in the editor"))),
        ),
        (error) => reportActionError("editor open", error),
      );
      return;
    }
    if (isPlainLetter(k, "s")) {
      // Live-environment open: the deployed SST stage when there is
      // one, else the running [dev_server] — the same either/or the
      // bolt badge renders.
      if (current.fields.deploy.data) {
        const url = stageUrl(current.wt.stage);
        if (!url) {
          rowLog.event.warn("no stage domain configured");
          return;
        }
        forkReported(openUrlHidingTerminal(url), (error) =>
          rowLog.debug(`open stage url failed: ${error.message}`),
        );
        rowLog.event.info(`opened ${current.wt.stage}`);
        return;
      }
      const dev = current.fields.dev.data;
      if (dev?.running && dev.url) {
        forkReported(openUrlHidingTerminal(dev.url), (error) =>
          rowLog.debug(`open dev url failed: ${error.message}`),
        );
        rowLog.event.info(`opened dev server (${dev.url})`);
        return;
      }
      rowLog.event.warn(
        config.devServer ? "no stage deployed / dev server running" : "not deployed",
      );
      return;
    }
    if (k.sequence === "y") {
      setModal({ kind: "yank", index: firstYankIndex(yankItemsFor(current)) });
      return;
    }
    if (isPlainLetter(k, "d")) {
      if (current.status.kind === StatusKind.Busy) {
        const label = current.status.op ?? current.status.label;
        toast(`${current.wt.slug} is ${label}`, theme.warn, 2000);
        return;
      }
      // Surface the same conditions `doRemove` guards on, at prompt
      // time — through the SAME helper, so the prompt can't promise a
      // removal the guard will refuse. When any hazard is present,
      // switch the confirm to a force-variant so the user can opt into
      // the destructive path inline instead of bouncing out to the shell.
      const hazards = destroyHazards(current);
      // Unknown ≠ clean: while dirty/sync are still loading nothing can
      // clear the row, and doRemove's non-force guard would refuse
      // anyway — offer the force variant with an honest caveat so the
      // user can proceed deliberately or wait a beat and re-prompt.
      const stateUnknown = hazards.some((h) => h.kind === "unknown");
      const reasons = hazards
        .filter((h) => h.kind !== "unknown")
        .map((h) => destroyHazardLabel(h));
      if (hazards.length > 0) {
        const lost =
          reasons.length > 0 ? `${reasons.join(", ")} will be lost.` : null;
        const caveat = stateUnknown
          ? "Dirty/unpushed state hasn't loaded yet — anything uncommitted or unpushed will be lost."
          : null;
        setModal({
          kind: "confirm",
          pendingKey: "d!",
          slug: current.wt.slug,
          target: selectedWorktree?.target,
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
          slug: current.wt.slug,
          target: selectedWorktree?.target,
          title: "remove worktree",
          message: `Remove ${current.wt.slug}?`,
          confirmLabel: "remove",
          danger: true,
        });
      }
      return;
    }
    if (isPlainLetter(k, "v")) {
      runAction("reviewer picker", () => openReviewerPicker(current.wt.slug));
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
        slug: current.wt.slug,
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
      const reviewer = config.github.reviewers
        ? config.github.defaultReviewer
        : null;
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
        slug: current.wt.slug,
        title: "ship PR",
        message: `Ship #${current.pr.number}? (${steps.join(", ")})`,
        confirmLabel: "ship",
      });
      return;
    }
    // Shift+M — the manager command palette — is handled in
    // global-keys.ts (it works without a selected row); the auto-merge
    // toggle that used to live here moved into the `!` picker.
    if (isPlainLetter(k, "f")) {
      // Tail the failing PR's `--log-failed` CI logs into the activity
      // pane. The flow refuses cleanly when checks aren't red.
      runAction("tail failed checks", () => doTailFailedChecks(current.wt.slug));
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
    if (isPlainLetter(k, "u")) {
      openStatusPicker();
      return;
    }
    // Expand/collapse the details pane's post-merge verification
    // steps. The field is dormant until the branch lands, so the block
    // starts closed on an unmerged row and open once the check is due;
    // this flips whichever of those applies. Toasting on a row with no
    // steps is the only discovery affordance the key has — the block
    // it acts on renders nothing at all when the field is absent.
    if (isShiftedLetter(k, "v")) {
      const steps = current.work?.verifyAfterMerge;
      if (!steps) {
        toast("no verify-after-merge steps on this row", theme.warn, 2000);
        return;
      }
      setVerifyExpanded(!(verifyExpanded ?? verifyStepsOpenByDefault(current)));
      return;
    }
    if (isPlainLetter(k, "t")) {
      if (!config.naming) {
        toast("worktree naming not configured", theme.warn, 2000);
        return;
      }
      if (current.status.kind === StatusKind.Busy) {
        toast(`${current.wt.slug} is busy`, theme.warn, 2000);
        return;
      }
      const slug = current.wt.slug;
      forkReported(
        keyPromise("refresh AI summary", () => refreshAiSummary(slug)).pipe(
          Effect.tap((ok) => Effect.sync(() => {
            if (ok) rowLog.event.dim("regenerating worktree summary");
            else toast("no diff context yet", theme.warn, 2000);
          })),
        ),
        (error) => reportActionError(error.label, error.cause),
      );
      return;
    }
}
