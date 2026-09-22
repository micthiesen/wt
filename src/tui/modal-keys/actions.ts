import type { KeyEvent } from "@opentui/core";

import { recentValues } from "../../core/actions.ts";
import { createLogger } from "../../core/logger.ts";
import { openInEditor } from "../../core/editor.ts";
import { operationErrors } from "../../core/errors.ts";
import { forkReported } from "../effect-boundary.ts";
import { printableMultiline } from "../app-helpers.ts";
import type { Modal } from "../modal-state.ts";
import { SESSION_SLOTS } from "../sessions/slots.ts";
import { applyEditKey, emptyEdit, insertText } from "../text-edit.tsx";
import type { SimpleModalContext } from "./ctx.ts";
import { handleListPickerKey } from "./list-picker.ts";
import { Effect } from "effect";

const io = operationErrors("modal-keys/actions");

/**
 * Fire-and-forget an action launch. `evaluate`'s declared return type
 * (`void | Promise<unknown>`) is deliberately loose in `ctx.ts` — the
 * real implementations always return a Promise — so `Promise.resolve`
 * normalizes either shape into one awaitable without duck-typing at
 * runtime.
 */
function launchFireAndForget(
  label: string,
  evaluate: () => void | Promise<unknown>,
  reportActionError: (label: string, err: unknown) => void,
): void {
  forkReported(
    io.promise(label, () => Promise.resolve(evaluate())),
    (error) => reportActionError(label, error),
  );
}

export function handleActionPickerKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "actionPicker" }>,
  ctx: SimpleModalContext,
): boolean {
  const {
    setModal,
    rows,
    buildActionPickerItems,
    buildManagerPickerItems,
    buildSlotPickerItems,
    canPickAction,
    launchAction,
    launchSlotCommand,
    doAutoMerge,
    toast,
    warnColor,
    reportActionError,
  } = ctx;
  const ap = modal.state;
  const manager = ap.surface === "manager";
  // Manager + slot palettes share the slot-delivery launch path
  // (`launchSlotCommand` — ap.slug is the slot slug on both surfaces);
  // only the manager also carries row-scoped entries.
  const palette = manager || ap.surface === "slot";
  const buildItems = () =>
    manager
      ? buildManagerPickerItems(ap.rowSlug)
      : ap.surface === "slot"
        ? buildSlotPickerItems()
        : buildActionPickerItems(ap.target!);
  if (ap.mode === "list") {
    const items = buildItems();
    const commitIndex = (i: number): void => {
      const item = items[i];
      if (!item) return;
      if (!canPickAction(item)) return;
      if (item.kind === "autoMerge") {
        // Direct toggle, no confirm — `!` + `m` is already deliberate,
        // matching the `; x` direct-kill convention.
        setModal(null);
        launchFireAndForget(
          "auto-merge",
          () => doAutoMerge(ap.slug, item.armed ? "disable" : "enable", ap.target),
          reportActionError,
        );
        return;
      }
      if (item.kind === "openEditor") {
        // Local flow like autoMerge — nothing injected. ap.slug is the
        // slot slug (the row only exists on the slot surface).
        setModal(null);
        const slot = SESSION_SLOTS.find((s) => s.slug === ap.slug);
        if (!slot) return;
        const editorLog = createLogger(slot.label);
        forkReported(
          openInEditor(slot.path).pipe(
            Effect.tap(() =>
              Effect.sync(() => editorLog.event.info(`opened ${slot.path}`)),
            ),
          ),
          (error) => reportActionError("editor open", error),
        );
        return;
      }
      if (item.kind === "devLogs") {
        setModal({ kind: "devLogs", slug: ap.slug, target: ap.target });
        return;
      }
      if (palette && item.kind === "action" && item.def.direct) {
        // Direct-launch builtins (the palettes' raw `/compact`): the
        // prompt IS the message; an extras screen would be noise. The
        // `palette` gate is load-bearing: this launch path is slot-
        // only, so a hypothetical future row-surface `direct` builtin
        // must NOT route here (it'd silently address the palette's
        // slot session).
        setModal(null);
        launchFireAndForget(
          item.def.name,
          () => launchSlotCommand(ap.slug, item.def, ""),
          reportActionError,
        );
        return;
      }
      // Fleet defs never take the argPicker: its launch path is
      // row-scoped (`launchAction`), which would either no-op on the
      // manager pseudo-slug or wrongly `[re:]`-prefix a fleet prompt.
      // No fleet builtin declares an argPrompt today; if one ever
      // does, it falls through to the extras editor instead.
      if (
        item.kind === "action" &&
        item.def.argPrompt &&
        !(palette && item.def.fleet)
      ) {
        const history = recentValues(item.def.id);
        setModal({
          kind: "argPicker",
          // Manager surface: arg-prompting defs are row-scoped user
          // actions; launch against the captured row (canPickAction
          // already blocked them when no row was selected).
          slug: manager ? (ap.rowSlug ?? ap.slug) : ap.slug,
          target: manager ? undefined : ap.target,
          def: item.def,
          history,
          index: 0,
          input: history.length === 0 ? emptyEdit : null,
        });
        return;
      }
      if (item.kind === "action" && item.def.kind === "shell") {
        setModal(null);
        launchFireAndForget(
          item.def.name,
          () =>
            launchAction(
              ap.slug,
              item.def,
              "",
              undefined,
              undefined,
              ap.target,
            ),
          reportActionError,
        );
        return;
      }
      const def = item.kind === "action" ? item.def : null;
      setModal({
        kind: "actionPicker",
        state: {
          mode: "edit",
          surface: ap.surface,
          slug: ap.slug,
          rowSlug: ap.rowSlug,
          target: ap.target,
          def: def && def.kind === "claude" ? def : null,
          extras: emptyEdit,
        },
      });
    };
    // Per-action letter keys (config-assigned) stay a pre-check: they
    // are dynamic per item list, with `c` reserved for the custom
    // prompt. Both follow the same "letter fires directly" convention
    // the shared handler implements for static chords.
    if (k.sequence === "c") {
      setModal({
        kind: "actionPicker",
        state: {
          mode: "edit",
          surface: ap.surface,
          slug: ap.slug,
          rowSlug: ap.rowSlug,
          target: ap.target,
          def: null,
          extras: emptyEdit,
        },
      });
      return true;
    }
    if (k.sequence && /^[a-z0-9]$/.test(k.sequence) && !k.ctrl && !k.meta && !k.shift) {
      const i = items.findIndex(
        (it) => it.kind !== "custom" && it.key === k.sequence,
      );
      if (i >= 0) {
        commitIndex(i);
        return true;
      }
    }
    // Chord-confirm: re-pressing the key that opened the surface
    // commits (modal UX rules). Slot palettes carry theirs on the slot
    // record; `<`/`>`/`\` arrive as plain sequences, so the shared
    // handler's sequence match covers them like `M` and `!`.
    const confirmKey =
      ap.surface === "slot"
        ? (SESSION_SLOTS.find((s) => s.slug === ap.slug)?.paletteKey ?? null)
        : manager
          ? "M"
          : "!";
    return handleListPickerKey(k, {
      count: items.length,
      index: ap.index,
      onMove: (next) =>
        setModal({ kind: "actionPicker", state: { ...ap, index: next } }),
      onCommit: commitIndex,
      onCancel: () => setModal(null),
      confirm: confirmKey ? [confirmKey] : [],
      // Actions are picked by their assigned letters, not positions.
      digits: false,
    });
  }

  if (k.ctrl && k.name === "c") {
    setModal(null);
    return true;
  }
  if (k.name === "escape") {
    const def = ap.def;
    if (def) {
      // Palette surfaces always have a list to return to; the row
      // surface guards against the worktree vanishing while the editor
      // was up.
      if (
        !palette &&
        ap.target?.location.kind !== "remote" &&
        !rows.find((r) => r.wt.slug === ap.slug)
      ) {
        setModal(null);
        toast("worktree gone", warnColor, 2000);
        return true;
      }
      const items = buildItems();
      const idx = items.findIndex(
        (it) => it.kind === "action" && it.def.id === def.id,
      );
      setModal({
        kind: "actionPicker",
        state: {
          mode: "list",
          surface: ap.surface,
          slug: ap.slug,
          rowSlug: ap.rowSlug,
          target: ap.target,
          index: Math.max(0, idx),
        },
      });
    } else {
      setModal(null);
    }
    return true;
  }
  if (k.name === "return") {
    const { slug, rowSlug, def, extras } = ap;
    setModal(null);
    if (palette) {
      // Fleet/slot builtins and free-text go straight to the palette's
      // slot session; the manager's row-scoped entries (ask-about-row,
      // user `target = "manager"` actions) ride the normal launch path
      // against the captured row so they get template vars and the
      // `[re: <slug>]` prefix.
      if (def === null || def.fleet)
        launchFireAndForget(
          def?.name ?? "custom message",
          () => launchSlotCommand(slug, def, extras.value),
          reportActionError,
        );
      else if (rowSlug)
        launchFireAndForget(
          def.name,
          () => launchAction(rowSlug, def, extras.value),
          reportActionError,
        );
      else toast("no row selected", warnColor, 2000);
    } else {
      launchFireAndForget(
        def?.name ?? "custom message",
        () =>
          launchAction(
            slug,
            def,
            extras.value,
            undefined,
            undefined,
            ap.target,
          ),
        reportActionError,
      );
    }
    return true;
  }
  // Cursor movement / deletion — shared editor logic (backspace
  // included; on an empty value it's a no-op, not a close).
  const edited = applyEditKey(k, ap.extras);
  if (edited) {
    setModal({ kind: "actionPicker", state: { ...ap, extras: edited } });
    return true;
  }
  const text = printableMultiline(k.sequence);
  if (text) {
    setModal({
      kind: "actionPicker",
      state: { ...ap, extras: insertText(ap.extras, text) },
    });
  }
  return true;
}

export function handleArgPickerKey(
  k: KeyEvent,
  modal: Extract<Modal, { kind: "argPicker" }>,
  { setModal, launchAction, reportActionError }: SimpleModalContext,
): boolean {
  const rowCount = modal.history.length + 1;
  const isInput = modal.input !== null;
  const launch = (value: string): void => {
    const trimmed = value.trim();
    if (!trimmed) return;
    setModal(null);
    launchFireAndForget(
      modal.def.name,
      () =>
        launchAction(
          modal.slug,
          modal.def,
          "",
          trimmed,
          undefined,
          modal.target,
        ),
      reportActionError,
    );
  };
  if (k.ctrl && k.name === "c") {
    setModal(null);
    return true;
  }
  if (isInput) {
    if (k.name === "escape") {
      if (modal.history.length > 0)
        setModal({ ...modal, input: null, index: 0 });
      else setModal(null);
      return true;
    }
    if (k.name === "return") {
      launch(modal.input?.value ?? "");
      return true;
    }
    const edited = applyEditKey(k, modal.input ?? emptyEdit);
    if (edited) {
      setModal({ ...modal, input: edited });
      return true;
    }
    const text = printableMultiline(k.sequence);
    if (text)
      setModal({ ...modal, input: insertText(modal.input ?? emptyEdit, text) });
    return true;
  }
  return handleListPickerKey(k, {
    count: rowCount,
    index: modal.index,
    onMove: (next) => setModal({ ...modal, index: next }),
    onCommit: (i) => {
      if (i >= modal.history.length) {
        setModal({ ...modal, input: emptyEdit });
        return;
      }
      const entry = modal.history[i];
      if (entry) launch(entry.value);
    },
    onCancel: () => setModal(null),
    // Digits launch history values only — the "+ new value…" row is
    // Enter-only (it opens an input, not a pick).
    digits: (n) => {
      const entry = modal.history[n - 1];
      if (entry) launch(entry.value);
    },
  });
}
