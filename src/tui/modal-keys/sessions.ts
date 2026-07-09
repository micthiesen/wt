import type { KeyEvent } from "@opentui/core";

import { nextAutoName, removeClaudeName, validateSessionName } from "../../core/harness/claude/names.ts";
import { getHarness, HARNESSES, type HarnessId } from "../../core/harness/index.ts";
import { sessionOutputId } from "../../core/outputs.ts";
import {
  closeHarnessSessionGracefully,
  killHarnessSession,
} from "../../core/tmux.ts";
import type { Modal } from "../modal.ts";
import { previewFocusPatch } from "../picker-preview.ts";
import { isSyntheticLiveSessionId } from "../hooks/useHarnessSessions.ts";
import type { SimpleModalContext } from "./ctx.ts";

export function handleClaudeSessionsPickerKey(
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

export function handleClaudeSessionsNewKey(
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

export function handleHarnessSelectKey(
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
