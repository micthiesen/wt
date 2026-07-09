import type { KeyEvent } from "@opentui/core";

import type { Modal } from "../modal.ts";
import type { SimpleModalContext } from "./ctx.ts";
import { handleHelpKey } from "./help.ts";
import {
  handleClaudeSessionsNewKey,
  handleClaudeSessionsPickerKey,
  handleHarnessSelectKey,
} from "./sessions.ts";
import { handleActionPickerKey, handleArgPickerKey } from "./actions.ts";
import { handleReviewerPickerKey } from "./reviewers.ts";
import { handleSectionPickerKey } from "./sections.ts";
import {
  handleBasePickerKey,
  handleBranchPickerKey,
  handleOutputsPickerKey,
} from "./pickers.ts";
import {
  handleCleanConfirmKey,
  handleConfirmKey,
  handleKillActionConfirmKey,
  handleKillSessionConfirmKey,
} from "./confirm.ts";
import { handleYankKey } from "./yank.ts";

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
