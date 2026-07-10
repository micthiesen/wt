import type { Dispatch, SetStateAction } from "react";
import type { KeyEvent } from "@opentui/core";

import type { ActionDef } from "../../core/actions.ts";
import type { HarnessId } from "../../core/harness/index.ts";
import type { Output } from "../../core/outputs.ts";
import type { RemovedWorktree } from "../../core/wtstate.ts";
import type { Modal } from "../modal-state.ts";
import type { PickerItem } from "../panels/action-picker.tsx";
import type { PickerRow } from "../panels/sessions-picker.tsx";
import type { SectionPickerItem } from "../panels/section-picker.tsx";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";

export type SimpleModalContext = {
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
