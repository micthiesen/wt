import type { Dispatch, SetStateAction } from "react";
import type { KeyEvent } from "@opentui/core";

import type { ActionDef } from "../core/actions.ts";
import { actionRegistry } from "../core/actions.ts";
import { recentValues } from "../core/action-history.ts";
import {
  nextAutoName,
  removeClaudeName,
  validateSessionName,
} from "../core/claude-sessions.ts";
import { getHarness, HARNESSES, type HarnessId } from "../core/harness/index.ts";
import { sessionOutputId, type Output } from "../core/outputs.ts";
import {
  closeHarnessSessionGracefully,
  killDiffSession,
  killHarnessSession,
  killShellSession,
} from "../core/tmux.ts";
import type { RemovedWorktree } from "../core/wtstate.ts";
import { isPlainLetter, printableMultiline, printableText } from "./app-helpers.ts";
import type { Modal } from "./modal.ts";
import { previewFocusPatch } from "./picker-preview.ts";
import type { PickerItem } from "./panels/action-picker.tsx";
import type { PickerRow } from "./panels/sessions-picker.tsx";
import { yankItemsFor } from "./panels/yank.tsx";
import type { SectionPickerItem } from "./panels/section-picker.tsx";
import { isSyntheticLiveSessionId } from "./hooks/useHarnessSessions.ts";
import type { WorktreeRow } from "./hooks/useWorktreeRows.ts";

type SimpleModalContext = {
  setModal: Dispatch<SetStateAction<Modal | null>>;
  current: WorktreeRow | undefined;
  refreshTmuxSessions: () => Promise<unknown>;
  commitBasePick: (
    item: { label: string; branch: string | null },
    slug: string,
  ) => void;
  doYank: (slug: string, label: string, value: string | null) => void;
  doClean: () => void;
  doRemove: (slug: string, opts?: { force?: boolean }) => Promise<void>;
  doAutoMerge: (slug: string, mode: "enable" | "disable") => Promise<void>;
  doMarkReady: (slug: string) => Promise<void>;
  doShipPr: (slug: string) => Promise<void>;
  doCheckoutReview: (branch: string) => Promise<void>;
  doRestoreRemoved: (entry: RemovedWorktree) => Promise<void>;
  clearAll: () => Promise<void>;
  submitReviewerPicker: () => Promise<void>;
  commitSectionPick: (item: SectionPickerItem, slug: string) => void;
  consumePrTargetChord: (k: KeyEvent) => boolean;
  setLastMoveTarget: Dispatch<SetStateAction<string | null>>;
  setSection: (slug: string, section: string | null) => Promise<unknown>;
  toast: (message: string, color?: string, ms?: number) => void;
  reportActionError: (label: string, err: unknown) => void;
  visibleOutputs: readonly Output[];
  currentSlug: string | undefined;
  setFocus: (slug: string | null, patch: { focused?: string | null }) => void;
  rows: readonly WorktreeRow[];
  buildActionPickerItems: (slug: string) => PickerItem[];
  canPickAction: (item: PickerItem) => boolean;
  // Return is deliberately loose: callers here fire-and-forget, and the
  // real impl returns a `LaunchOutcome` the automations engine consumes.
  launchAction: (
    slug: string,
    def: ActionDef | null,
    extras: string,
    arg?: string,
  ) => void | Promise<unknown>;
  doSpawnNamedClaudeSession: (slug: string, name: string) => void;
  doEnterHarnessSession: (
    slug: string,
    harnessId: HarnessId,
    opts: Record<string, unknown>,
  ) => void;
  pickerRows: ReadonlyArray<PickerRow>;
  doKillClaudeSession: (slug: string, name: string | null) => void;
  refreshHarnessSessions: (slug: string) => Promise<unknown>;
  refreshClaudeSummaries: (slug: string) => Promise<unknown>;
  infoColor: string;
  fgDimColor: string;
  warnColor: string;
  logInfo: (message: string) => void;
  logWarn: (message: string) => void;
  logErr: (message: string) => void;
};

export function handleSimpleModalKey(
  k: KeyEvent,
  modal: Modal,
  ctx: SimpleModalContext,
): boolean {
  switch (modal.kind) {
    case "help":
      return handleHelpKey(k, modal, ctx);
    case "reviewerPicker":
      return handleReviewerPickerKey(k, modal, ctx);
    case "sectionPicker":
      return handleSectionPickerKey(k, modal, ctx);
    case "outputsPicker":
      return handleOutputsPickerKey(k, modal, ctx);
    case "actionPicker":
      return handleActionPickerKey(k, modal, ctx);
    case "argPicker":
      return handleArgPickerKey(k, modal, ctx);
    case "claudeSessionsNew":
      return handleClaudeSessionsNewKey(k, modal, ctx);
    case "harnessSelect":
      return handleHarnessSelectKey(k, modal, ctx);
    case "claudeSessionsPicker":
      return handleClaudeSessionsPickerKey(k, modal, ctx);
    case "killActionConfirm":
      return handleKillActionConfirmKey(k, modal, ctx);
    case "killSessionConfirm":
      return handleKillSessionConfirmKey(k, modal, ctx);
    case "branchPicker":
      return handleBranchPickerKey(k, modal, ctx);
    case "basePicker":
      return handleBasePickerKey(k, modal, ctx);
    case "yank":
      return handleYankKey(k, ctx);
    case "cleanConfirm":
      return handleCleanConfirmKey(k, ctx);
    case "confirm":
      return handleConfirmKey(k, modal, ctx);
    default:
      return false;
  }
}

function handleHelpKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "help" }>,
  { setModal }: SimpleModalContext,
): boolean {
  if (modal.searching) {
    if (k.ctrl && k.name === "c") {
      setModal(null);
      return true;
    }
    if (k.name === "escape") {
      setModal({ ...modal, searching: false, query: "" });
      return true;
    }
    if (k.name === "return") {
      setModal({ ...modal, searching: false });
      return true;
    }
    if (k.name === "backspace") {
      if (modal.query.length === 0) {
        setModal({ ...modal, searching: false });
        return true;
      }
      setModal({ ...modal, query: modal.query.slice(0, -1) });
      return true;
    }
    const text = printableText(k.sequence);
    if (text) setModal({ ...modal, query: modal.query + text });
    return true;
  }
  if (k.sequence === "/") {
    setModal({ ...modal, searching: true });
    return true;
  }
  if (k.name === "escape" && modal.query) {
    setModal({ ...modal, query: "" });
    return true;
  }
  if (
    k.name === "escape" ||
    k.sequence === "?" ||
    k.name === "q" ||
    (k.ctrl && k.name === "c")
  ) {
    setModal(null);
  }
  return true;
}

function handleClaudeSessionsPickerKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "claudeSessionsPicker" }>,
  ctx: SimpleModalContext,
): boolean {
  const {
    setModal,
    pickerRows,
    setFocus,
    doEnterHarnessSession,
    doKillClaudeSession,
    refreshTmuxSessions,
    refreshHarnessSessions,
    refreshClaudeSummaries,
    toast,
    reportActionError,
    fgDimColor,
    logInfo,
    logWarn,
  } = ctx;
  const slug = modal.slug;
  const rowsLocal = pickerRows;
  const totalRows = rowsLocal.length;
  const idx = Math.min(Math.max(0, modal.index), Math.max(0, totalRows - 1));
  const previewIdFor = (i: number): string | null => {
    const r = rowsLocal[i];
    if (!r || r.kind !== "session") return null;
    if (!r.entry.isLive) return null;
    if (r.entry.harnessId !== "claude") return null;
    return sessionOutputId(slug, "claude", r.entry.extras.managedName);
  };
  const moveTo = (next: number): void => {
    setModal({ ...modal, index: next });
    const patch = previewFocusPatch(previewIdFor(next));
    if (patch) setFocus(slug, patch);
  };
  const openNewClaude = (): void => {
    setModal({ kind: "claudeSessionsNew", slug, input: "", error: null });
  };
  const commitRow = (i: number): void => {
    const r = rowsLocal[i];
    if (!r) return;
    if (r.kind === "new") {
      if (r.harnessId === "claude") openNewClaude();
      else {
        setModal(null);
        doEnterHarnessSession(slug, r.harnessId, { freshSlot: true });
      }
      return;
    }
    const e = r.entry;
    const isSyntheticLive = isSyntheticLiveSessionId(e.sessionId);
    const resumeSessionId =
      e.isLive || isSyntheticLive ? null : e.sessionId;
    const freshSlot =
      getHarness(e.harnessId).singleSlot && resumeSessionId !== null;
    setModal(null);
    doEnterHarnessSession(slug, e.harnessId, {
      managedName: e.extras.managedName,
      resumeSessionId,
      freshSlot,
    });
  };
  const jumpToNew = (harnessId: HarnessId): void => {
    const target = rowsLocal.findIndex(
      (r) => r.kind === "new" && r.harnessId === harnessId,
    );
    if (target >= 0) moveTo(target);
  };
  if (k.name === "j" || k.name === "down") {
    moveTo(Math.min(idx + 1, totalRows - 1));
    return true;
  }
  if (k.name === "k" || k.name === "up") {
    moveTo(Math.max(0, idx - 1));
    return true;
  }
  if (k.sequence && /^[1-9]$/.test(k.sequence)) {
    const n = parseInt(k.sequence, 10) - 1;
    let cursor = 0;
    for (let i = 0; i < rowsLocal.length; i++) {
      if (rowsLocal[i]!.kind !== "session") continue;
      if (cursor === n) {
        commitRow(i);
        return true;
      }
      cursor++;
    }
    return true;
  }
  if (k.sequence === "x") {
    const r = rowsLocal[idx];
    if (r?.kind === "session") {
      const e = r.entry;
      if (e.harnessId === "claude") {
        if (e.isLive) {
          doKillClaudeSession(slug, e.extras.managedName);
        } else if (e.extras.managedName !== null) {
          removeClaudeName(slug, e.extras.managedName);
          void refreshClaudeSummaries(slug);
          logInfo(`forgot ghost session "${e.extras.managedName}" on ${slug}`);
        }
        setModal(null);
      } else if (e.isLive) {
        void (async () => {
          await killHarnessSession(slug, e.harnessId);
          await Promise.all([
            refreshTmuxSessions(),
            refreshHarnessSessions(slug),
          ]);
          logWarn(`killed ${getHarness(e.harnessId).label} session on ${slug}`);
        })();
        setModal(null);
      } else {
        toast(
          `${getHarness(e.harnessId).label} session is dead; remove via ${e.harnessId} CLI`,
          fgDimColor,
          2000,
        );
      }
      return true;
    }
  }
  if (k.sequence === "d" && !k.ctrl && !k.meta) {
    const r = rowsLocal[idx];
    if (r?.kind === "session") {
      const e = r.entry;
      if (!e.isLive) {
        toast("session isn't live, nothing to close", fgDimColor, 1500);
        return true;
      }
      logInfo(`closing ${getHarness(e.harnessId).label} session on ${slug} (ctrl+d x2)`);
      void closeHarnessSessionGracefully(
        slug,
        e.harnessId,
        e.extras.managedName,
      ).then(
        () =>
          setTimeout(() => {
            void refreshTmuxSessions();
            void refreshHarnessSessions(slug);
          }, 800),
        (err) => reportActionError("close session", err),
      );
      setModal(null);
    }
    return true;
  }
  for (const h of HARNESSES) {
    if (k.sequence === h.letter && !k.shift && !k.ctrl && !k.meta) {
      jumpToNew(h.id);
      return true;
    }
  }
  if (k.sequence === ";" || k.name === "return") {
    commitRow(idx);
    return true;
  }
  if (
    k.name === "escape" ||
    k.sequence === "q" ||
    (k.ctrl && k.name === "c")
  ) {
    setModal(null);
  }
  return true;
}

function handleActionPickerKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "actionPicker" }>,
  ctx: SimpleModalContext,
): boolean {
  const {
    setModal,
    rows,
    buildActionPickerItems,
    canPickAction,
    launchAction,
    toast,
    warnColor,
  } = ctx;
  const ap = modal.state;
  if (ap.mode === "list") {
    const items = buildActionPickerItems(ap.slug);
    if (k.name === "j" || k.name === "down") {
      setModal({
        kind: "actionPicker",
        state: { ...ap, index: Math.min(ap.index + 1, items.length - 1) },
      });
      return true;
    }
    if (k.name === "k" || k.name === "up") {
      setModal({
        kind: "actionPicker",
        state: { ...ap, index: Math.max(ap.index - 1, 0) },
      });
      return true;
    }
    const commitIndex = (i: number): void => {
      const item = items[i];
      if (!item) return;
      if (!canPickAction(item)) return;
      if (item.kind === "action" && item.def.argPrompt) {
        const history = recentValues(item.def.id);
        setModal({
          kind: "argPicker",
          slug: ap.slug,
          def: item.def,
          history,
          index: 0,
          input: history.length === 0 ? "" : null,
        });
        return;
      }
      if (item.kind === "action" && item.def.kind === "shell") {
        setModal(null);
        void launchAction(ap.slug, item.def, "");
        return;
      }
      const def = item.kind === "action" ? item.def : null;
      setModal({
        kind: "actionPicker",
        state: {
          mode: "edit",
          slug: ap.slug,
          def: def && def.kind === "claude" ? def : null,
          extras: "",
        },
      });
    };
    if (k.sequence === "c") {
      setModal({
        kind: "actionPicker",
        state: { mode: "edit", slug: ap.slug, def: null, extras: "" },
      });
      return true;
    }
    if (k.sequence && /^[a-z]$/.test(k.sequence)) {
      const i = items.findIndex(
        (it) => it.kind === "action" && it.key === k.sequence,
      );
      if (i >= 0) {
        commitIndex(i);
        return true;
      }
    }
    if (k.name === "return" || k.sequence === "!") {
      commitIndex(ap.index);
      return true;
    }
    if (
      k.name === "escape" ||
      k.sequence === "q" ||
      (k.ctrl && k.name === "c")
    ) {
      setModal(null);
    }
    return true;
  }

  if (k.ctrl && k.name === "c") {
    setModal(null);
    return true;
  }
  if (k.name === "escape") {
    const def = ap.def;
    if (def) {
      if (!rows.find((r) => r.wt.slug === ap.slug)) {
        setModal(null);
        toast("worktree gone", warnColor, 2000);
        return true;
      }
      const items = buildActionPickerItems(ap.slug);
      const idx = items.findIndex(
        (it) => it.kind === "action" && it.def.id === def.id,
      );
      setModal({
        kind: "actionPicker",
        state: { mode: "list", slug: ap.slug, index: Math.max(0, idx) },
      });
    } else {
      setModal(null);
    }
    return true;
  }
  if (k.name === "return") {
    const { slug, def, extras } = ap;
    setModal(null);
    void launchAction(slug, def, extras);
    return true;
  }
  if (k.name === "backspace") {
    if (ap.extras.length === 0) return true;
    setModal({
      kind: "actionPicker",
      state: { ...ap, extras: ap.extras.slice(0, -1) },
    });
    return true;
  }
  const text = printableMultiline(k.sequence);
  if (text) {
    setModal({
      kind: "actionPicker",
      state: { ...ap, extras: ap.extras + text },
    });
  }
  return true;
}

function handleArgPickerKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "argPicker" }>,
  { setModal, launchAction }: SimpleModalContext,
): boolean {
  const rowCount = modal.history.length + 1;
  const isInput = modal.input !== null;
  const launch = (value: string): void => {
    const trimmed = value.trim();
    if (!trimmed) return;
    setModal(null);
    void launchAction(modal.slug, modal.def, "", trimmed);
  };
  if (k.ctrl && k.name === "c") {
    setModal(null);
    return true;
  }
  if (isInput) {
    if (k.name === "escape") {
      if (modal.history.length > 0) setModal({ ...modal, input: null, index: 0 });
      else setModal(null);
      return true;
    }
    if (k.name === "return") {
      launch(modal.input ?? "");
      return true;
    }
    if (k.name === "backspace") {
      setModal({ ...modal, input: (modal.input ?? "").slice(0, -1) });
      return true;
    }
    const text = printableMultiline(k.sequence);
    if (text) setModal({ ...modal, input: (modal.input ?? "") + text });
    return true;
  }
  if (k.name === "escape" || k.sequence === "q") {
    setModal(null);
    return true;
  }
  if (k.name === "j" || k.name === "down") {
    setModal({ ...modal, index: Math.min(modal.index + 1, rowCount - 1) });
    return true;
  }
  if (k.name === "k" || k.name === "up") {
    setModal({ ...modal, index: Math.max(modal.index - 1, 0) });
    return true;
  }
  if (k.sequence && /^[1-9]$/.test(k.sequence)) {
    const i = Number(k.sequence) - 1;
    if (i < modal.history.length) {
      const entry = modal.history[i];
      if (entry) launch(entry.value);
    }
    return true;
  }
  if (k.name === "return") {
    if (modal.index >= modal.history.length) {
      setModal({ ...modal, input: "" });
      return true;
    }
    const entry = modal.history[modal.index];
    if (entry) launch(entry.value);
  }
  return true;
}

function handleClaudeSessionsNewKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "claudeSessionsNew" }>,
  { setModal, doSpawnNamedClaudeSession }: SimpleModalContext,
): boolean {
  if (k.name === "escape") {
    setModal({ kind: "claudeSessionsPicker", slug: modal.slug, index: 0 });
    return true;
  }
  if (k.ctrl && k.name === "c") {
    setModal(null);
    return true;
  }
  if (k.name === "return") {
    const trimmed = modal.input.trim();
    const name = trimmed === "" ? nextAutoName(modal.slug) : trimmed;
    const err = validateSessionName(name);
    if (err) {
      setModal({ ...modal, error: err });
      return true;
    }
    setModal(null);
    doSpawnNamedClaudeSession(modal.slug, name);
    return true;
  }
  if (k.name === "backspace") {
    if (modal.input.length === 0) {
      setModal({ kind: "claudeSessionsPicker", slug: modal.slug, index: 0 });
      return true;
    }
    setModal({ ...modal, input: modal.input.slice(0, -1), error: null });
    return true;
  }
  if (k.sequence && /^[a-zA-Z0-9_-]$/.test(k.sequence)) {
    setModal({ ...modal, input: modal.input + k.sequence, error: null });
  }
  return true;
}

function handleHarnessSelectKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "harnessSelect" }>,
  { setModal, doSpawnNamedClaudeSession, doEnterHarnessSession }: SimpleModalContext,
): boolean {
  const idx = Math.min(Math.max(0, modal.index), HARNESSES.length - 1);
  const slug = modal.slug;
  const commit = (chosen: HarnessId): void => {
    setModal(null);
    if (chosen === "claude") {
      doSpawnNamedClaudeSession(slug, nextAutoName(slug));
    } else {
      doEnterHarnessSession(slug, chosen, {});
    }
  };
  if (k.name === "j" || k.name === "down") {
    setModal({ ...modal, index: Math.min(idx + 1, HARNESSES.length - 1) });
    return true;
  }
  if (k.name === "k" || k.name === "up") {
    setModal({ ...modal, index: Math.max(0, idx - 1) });
    return true;
  }
  const letterMatch = HARNESSES.find(
    (h) => k.sequence === h.letter && !k.shift && !k.ctrl && !k.meta,
  );
  if (letterMatch) {
    commit(letterMatch.id);
    return true;
  }
  if ((k.name === "f12" && !k.shift) || k.name === "return") {
    commit(HARNESSES[idx]!.id);
    return true;
  }
  if (
    k.name === "escape" ||
    k.sequence === "q" ||
    (k.ctrl && k.name === "c")
  ) {
    setModal(null);
  }
  return true;
}

function handleReviewerPickerKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "reviewerPicker" }>,
  { setModal, submitReviewerPicker }: SimpleModalContext,
): boolean {
  if (k.name === "j" || k.name === "down") {
    setModal({ ...modal, index: Math.min(modal.index + 1, modal.items.length - 1) });
    return true;
  }
  if (k.name === "k" || k.name === "up") {
    setModal({ ...modal, index: Math.max(modal.index - 1, 0) });
    return true;
  }
  if (k.name === "space" || k.sequence === " ") {
    const item = modal.items[modal.index];
    if (item) {
      const next = new Set(modal.checked);
      if (next.has(item.key)) next.delete(item.key);
      else next.add(item.key);
      setModal({ ...modal, checked: next });
    }
    return true;
  }
  if (k.name === "return" || k.sequence === "v") {
    void submitReviewerPicker();
    return true;
  }
  if (
    k.name === "escape" ||
    k.sequence === "q" ||
    (k.ctrl && k.name === "c")
  ) {
    setModal(null);
  }
  return true;
}

function handleSectionPickerKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "sectionPicker" }>,
  ctx: SimpleModalContext,
): boolean {
  const {
    setModal,
    consumePrTargetChord,
    setSection,
    setLastMoveTarget,
    toast,
    reportActionError,
    commitSectionPick,
    infoColor,
  } = ctx;
  if (consumePrTargetChord(k)) {
    setModal(null);
    return true;
  }
  if (modal.newName !== null) {
    if (k.name === "escape") {
      setModal({ ...modal, newName: null });
      return true;
    }
    if (k.ctrl && k.name === "c") {
      setModal(null);
      return true;
    }
    if (k.name === "return") {
      const name = modal.newName.trim();
      if (!name) {
        setModal({ ...modal, newName: null });
        return true;
      }
      const slug = modal.slug;
      setSection(slug, name).then(
        () => toast(`moved to ${name}`, infoColor, 1500),
        (err) => reportActionError("move", err),
      );
      setLastMoveTarget(name);
      setModal(null);
      return true;
    }
    if (k.name === "backspace") {
      if (modal.newName.length === 0) {
        setModal({ ...modal, newName: null });
        return true;
      }
      setModal({ ...modal, newName: modal.newName.slice(0, -1) });
      return true;
    }
    const text = printableText(k.sequence);
    if (text) setModal({ ...modal, newName: modal.newName + text });
    return true;
  }
  if (k.name === "j" || k.name === "down") {
    setModal({ ...modal, index: Math.min(modal.index + 1, modal.items.length - 1) });
    return true;
  }
  if (k.name === "k" || k.name === "up") {
    setModal({ ...modal, index: Math.max(modal.index - 1, 0) });
    return true;
  }
  if (isPlainLetter(k, "n")) {
    const createIdx = modal.items.findIndex((it) => it.kind === "create");
    if (createIdx >= 0) commitSectionPick(modal.items[createIdx]!, modal.slug);
    return true;
  }
  if (k.sequence && /^[1-9]$/.test(k.sequence)) {
    const i = parseInt(k.sequence, 10) - 1;
    const item = modal.items[i];
    if (item && item.kind !== "create") commitSectionPick(item, modal.slug);
    return true;
  }
  if (k.name === "return" || isPlainLetter(k, "l")) {
    const item = modal.items[modal.index];
    if (item) commitSectionPick(item, modal.slug);
    return true;
  }
  if (
    k.name === "escape" ||
    k.sequence === "q" ||
    (k.ctrl && k.name === "c")
  ) {
    setModal(null);
  }
  return true;
}

function handleOutputsPickerKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "outputsPicker" }>,
  { setModal, visibleOutputs, currentSlug, setFocus }: SimpleModalContext,
): boolean {
  const idx =
    visibleOutputs.length === 0
      ? 0
      : Math.min(Math.max(0, modal.index), visibleOutputs.length - 1);
  const moveTo = (next: number): void => {
    setModal({ kind: "outputsPicker", index: next });
    const patch = previewFocusPatch(visibleOutputs[next]?.id ?? null);
    if (patch) setFocus(currentSlug ?? null, patch);
  };
  const commit = (i: number): void => {
    const target = visibleOutputs[i];
    if (target) setFocus(currentSlug ?? null, { focused: target.id });
    setModal(null);
  };
  if (k.name === "j" || k.name === "down") {
    moveTo(Math.min(idx + 1, visibleOutputs.length - 1));
    return true;
  }
  if (k.name === "k" || k.name === "up") {
    moveTo(Math.max(0, idx - 1));
    return true;
  }
  if (k.sequence && /^[1-9]$/.test(k.sequence)) {
    const i = parseInt(k.sequence, 10) - 1;
    if (visibleOutputs[i]) commit(i);
    return true;
  }
  if (k.sequence === "'" || k.name === "return") {
    commit(idx);
    return true;
  }
  if (
    k.name === "escape" ||
    k.sequence === "q" ||
    (k.ctrl && k.name === "c")
  ) {
    setModal(null);
  }
  return true;
}

function handleKillActionConfirmKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "killActionConfirm" }>,
  { setModal, logWarn }: SimpleModalContext,
): boolean {
  if (k.name === "y" || k.name === "return") {
    const { slug, actionName } = modal;
    setModal(null);
    void actionRegistry.kill(slug).then((killed) => {
      if (killed) logWarn(`killed action "${actionName}" on ${slug}`);
    });
    return true;
  }
  if (
    k.name === "n" ||
    k.name === "escape" ||
    k.sequence === "!" ||
    k.sequence === "q" ||
    (k.ctrl && k.name === "c")
  ) {
    setModal(null);
  }
  return true;
}

function handleKillSessionConfirmKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "killSessionConfirm" }>,
  { setModal, refreshTmuxSessions, logWarn, logErr }: SimpleModalContext,
): boolean {
  if (k.name === "y" || k.name === "return") {
    const { slug, sessionKind } = modal;
    setModal(null);
    const kill = sessionKind === "diff" ? killDiffSession : killShellSession;
    void kill(slug)
      .then(() => {
        logWarn(`killed ${sessionKind} session on ${slug}`);
        void refreshTmuxSessions();
      })
      .catch((err) => {
        const msg = err instanceof Error ? err.message : String(err);
        logErr(`kill ${sessionKind} session failed for ${slug}: ${msg}`);
      });
    return true;
  }
  if (
    k.name === "n" ||
    k.name === "escape" ||
    k.sequence === "q" ||
    (k.ctrl && k.name === "c")
  ) {
    setModal(null);
  }
  return true;
}

function handleBranchPickerKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "branchPicker" }>,
  { setModal }: SimpleModalContext,
): boolean {
  if (k.name === "j" || k.name === "down") {
    setModal({ ...modal, index: Math.min(modal.index + 1, modal.items.length - 1) });
    return true;
  }
  if (k.name === "k" || k.name === "up") {
    setModal({ ...modal, index: Math.max(modal.index - 1, 0) });
    return true;
  }
  if (k.name === "return") {
    const chosen = modal.items[modal.index]!;
    modal.resolve(chosen);
    setModal(null);
    return true;
  }
  if (
    k.name === "escape" ||
    k.sequence === "q" ||
    (k.ctrl && k.name === "c")
  ) {
    modal.resolve(null);
    setModal(null);
  }
  return true;
}

function handleBasePickerKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "basePicker" }>,
  { setModal, commitBasePick }: SimpleModalContext,
): boolean {
  if (k.name === "j" || k.name === "down") {
    setModal({ ...modal, index: Math.min(modal.index + 1, modal.items.length - 1) });
    return true;
  }
  if (k.name === "k" || k.name === "up") {
    setModal({ ...modal, index: Math.max(modal.index - 1, 0) });
    return true;
  }
  if (k.sequence && /^[1-9]$/.test(k.sequence)) {
    const item = modal.items[parseInt(k.sequence, 10) - 1];
    if (item) commitBasePick(item, modal.slug);
    return true;
  }
  if (k.name === "return" || isPlainLetter(k, "b")) {
    const item = modal.items[modal.index];
    if (item) commitBasePick(item, modal.slug);
    return true;
  }
  if (
    k.name === "escape" ||
    k.sequence === "q" ||
    (k.ctrl && k.name === "c")
  ) {
    setModal(null);
  }
  return true;
}

function handleYankKey(
  k: KeyEvent,
  { setModal, current, doYank }: SimpleModalContext,
): boolean {
  if (
    k.name === "escape" ||
    k.sequence === "y" ||
    k.sequence === "q" ||
    (k.ctrl && k.name === "c")
  ) {
    setModal(null);
    return true;
  }
  if (current) {
    const item = yankItemsFor(current).find((it) => it.key === k.sequence);
    if (item) {
      setModal(null);
      doYank(current.wt.slug, item.label, item.value);
    }
  }
  return true;
}

function handleCleanConfirmKey(
  k: KeyEvent,
  { setModal, doClean }: SimpleModalContext,
): boolean {
  if (k.name === "y" || k.name === "return") {
    setModal(null);
    void doClean();
    return true;
  }
  if (
    k.name === "n" ||
    k.name === "escape" ||
    k.sequence === "q" ||
    (k.ctrl && k.name === "c")
  ) {
    setModal(null);
  }
  return true;
}

function handleConfirmKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "confirm" }>,
  ctx: SimpleModalContext,
): boolean {
  const {
    setModal,
    current,
    doRemove,
    doAutoMerge,
    doMarkReady,
    doShipPr,
    doCheckoutReview,
    doRestoreRemoved,
    clearAll,
    logWarn,
  } = ctx;
  if (k.name === "y" || k.name === "return") {
    const pending = modal.pendingKey;
    setModal(null);
    if (pending === "d" && current) {
      void doRemove(current.wt.slug);
    } else if (pending === "d!" && current) {
      void doRemove(current.wt.slug, { force: true });
    } else if (pending === "m+" && current) {
      void doAutoMerge(current.wt.slug, "enable");
    } else if (pending === "m-" && current) {
      void doAutoMerge(current.wt.slug, "disable");
    } else if (pending === "e" && current) {
      void doMarkReady(current.wt.slug);
    } else if (pending === "E" && current) {
      void doShipPr(current.wt.slug);
    } else if (pending === "review-wt" && modal.reviewBranch) {
      void doCheckoutReview(modal.reviewBranch);
    } else if (pending === "restore" && modal.restoreEntry) {
      void doRestoreRemoved(modal.restoreEntry);
    } else if (pending === "R") {
      logWarn("cleared all cached data; refetching from scratch");
      void clearAll();
    }
    return true;
  }
  if (
    k.name === "n" ||
    k.name === "escape" ||
    k.sequence === "q" ||
    (k.ctrl && k.name === "c")
  ) {
    setModal(null);
  }
  return true;
}
