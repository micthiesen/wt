import type { ActionDef } from "../core/actions.ts";
import type { HistoryEntry } from "../core/action-history.ts";
import type { ActionPickerState } from "./panels/action-picker.tsx";
import type { MultiPickerItem } from "./panels/picker.tsx";
import type { SectionPickerItem } from "./panels/section-picker.tsx";

/**
 * Every overlay/modal the TUI can display. Exactly one is active at a
 * time (or `null`); keyboard handling dispatches by `kind` and JSX renders by
 * `kind`.
 */
export type Modal =
  | { kind: "help"; query: string; searching: boolean }
  | { kind: "cleanConfirm" }
  | {
      kind: "confirm";
      pendingKey: string;
      title: string;
      message: string;
      detail?: string;
      confirmLabel?: string;
      danger?: boolean;
      reviewBranch?: string;
    }
  | { kind: "yank" }
  | {
      kind: "branchPicker";
      title: string;
      items: string[];
      index: number;
      resolve: (picked: string | null) => void;
    }
  | {
      kind: "basePicker";
      slug: string;
      items: Array<{ label: string; branch: string | null }>;
      index: number;
    }
  | {
      kind: "reviewerPicker";
      title: string;
      items: MultiPickerItem[];
      index: number;
      checked: Set<string>;
      original: Set<string>;
      slug: string;
      prNumber: number;
    }
  | {
      kind: "sectionPicker";
      title: string;
      slug: string;
      items: SectionPickerItem[];
      index: number;
      newName: string | null;
    }
  | { kind: "actionPicker"; state: ActionPickerState }
  | {
      kind: "argPicker";
      slug: string;
      def: ActionDef;
      history: readonly HistoryEntry[];
      index: number;
      input: string | null;
    }
  | { kind: "outputsPicker"; index: number }
  | { kind: "claudeSessionsPicker"; slug: string; index: number }
  | {
      kind: "claudeSessionsNew";
      slug: string;
      input: string;
      error: string | null;
    }
  | { kind: "harnessSelect"; slug: string; index: number }
  | { kind: "killActionConfirm"; slug: string; actionName: string }
  | {
      kind: "killSessionConfirm";
      slug: string;
      sessionKind: "shell" | "diff";
    };
