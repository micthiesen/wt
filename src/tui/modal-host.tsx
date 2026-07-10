/**
 * Modal overlay rendering, extracted from `app.tsx`. Two components
 * because render order is paint order in OpenTUI: the bottom-pane
 * pickers (`PreFooterModals`) mount BEFORE the `Footer` so its hint
 * line stays on top of them, while the centered overlays
 * (`PostFooterModals`) mount after it. Which component owns a modal
 * kind is part of the existing visual contract — don't shuffle kinds
 * between them.
 */
import { nextAutoName } from "../core/harness/claude/names.ts";
import { HARNESSES, type HarnessId } from "../core/harness/index.ts";
import { actionSkillPrefix, buildActionVars } from "./app-helpers.ts";
import type { Modal } from "./modal.ts";
import {
  ActionEditModal,
  ActionPickerModal,
  type PickerItem,
} from "./panels/action-picker.tsx";
import { CleanConfirmModal } from "./panels/clean-confirm.tsx";
import { ConfirmModal } from "./panels/confirm-modal.tsx";
import { HarnessPickerModal } from "./panels/harness-picker.tsx";
import { HelpOverlay } from "./panels/help.tsx";
import { KillActionConfirmModal } from "./panels/kill-action-confirm.tsx";
import { KillSessionConfirmModal } from "./panels/kill-session-confirm.tsx";
import { OutputsPicker } from "./panels/outputs-picker.tsx";
import { ArgPickerModal, MultiPickerModal, PickerModal } from "./panels/picker.tsx";
import { SectionPickerModal } from "./panels/section-picker.tsx";
import {
  SessionsPickerList,
  SessionsPickerNew,
  type PickerRow,
} from "./panels/sessions-picker.tsx";
import { YankModal } from "./panels/yank.tsx";
import type { useOutputFocus } from "./hooks/useOutputFocus.ts";
import type { WorktreeRow } from "./hooks/useWorktreeRows.ts";

/** Bottom-pane pickers rendered between the OutputViewer and the Footer. */
export function PreFooterModals({
  modal,
  currentSlug,
  visibleOutputs,
  pickerRows,
  pickerSummaries,
}: {
  modal: Modal | null;
  currentSlug: string | undefined;
  visibleOutputs: ReturnType<typeof useOutputFocus>["visibleOutputs"];
  pickerRows: ReadonlyArray<PickerRow>;
  pickerSummaries: Map<string, { text: string } | null>;
}) {
  return (
    <>
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
    </>
  );
}

/** Centered overlays rendered after the Footer. */
export function PostFooterModals({
  modal,
  current,
  rows,
  cleanCandidates,
  primaryHarness,
  buildActionPickerItems,
}: {
  modal: Modal | null;
  current: WorktreeRow | undefined;
  rows: WorktreeRow[];
  cleanCandidates: WorktreeRow[];
  primaryHarness: HarnessId;
  buildActionPickerItems: (slug: string) => PickerItem[];
}) {
  return (
    <>
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
    </>
  );
}
