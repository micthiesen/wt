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
import { VISIBLE_HARNESSES, type HarnessId } from "../core/harness/index.ts";
import type { PerfSnapshot } from "../core/perf.ts";
import { actionSkillPrefix, buildActionVars } from "./app-helpers.ts";
import {
  actionSubjectVars,
  type ActionSubjectResolver,
} from "./action-subject.ts";
import { workStateColor, workStateGlyph } from "./badges.ts";
import type { Modal } from "./modal-state.ts";
import {
  ActionEditModal,
  ActionPickerModal,
  type PickerItem,
} from "./panels/action-picker.tsx";
import { CleanConfirmModal } from "./panels/clean-confirm.tsx";
import { ConfirmModal } from "./panels/confirm-modal.tsx";
import { DevLogsOverlay } from "./panels/dev-logs-overlay.tsx";
import { ErrorOverlay } from "./panels/error-overlay.tsx";
import { HarnessPickerModal } from "./panels/harness-picker.tsx";
import { HelpOverlay } from "./panels/help.tsx";
import { KillActionConfirmModal } from "./panels/kill-action-confirm.tsx";
import { KillSessionConfirmModal } from "./panels/kill-session-confirm.tsx";
import { OutputsPicker } from "./panels/outputs-picker.tsx";
import { PerfOverlay } from "./panels/perf.tsx";
import { ArgPickerModal, MultiPickerModal, PickerModal } from "./panels/picker.tsx";
import { SectionPickerModal } from "./panels/section-picker.tsx";
import {
  SessionsPickerList,
  SessionsPickerNew,
  type PickerRow,
} from "./panels/sessions-picker.tsx";
import { sectionYankItems, YankModal, yankItemsFor } from "./panels/yank.tsx";
import type { SelectedSection } from "./hooks/useVisualItems.ts";
import type { useOutputFocus } from "./hooks/useOutputFocus.ts";
import type { WorktreeRow } from "./hooks/useWorktreeRows.ts";
import type { CleanCandidate } from "./clean-candidate.ts";
import type { WorktreeTarget } from "../core/worktree-target.ts";

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
            VISIBLE_HARNESSES.length - 1,
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
  selectedSection,
  rows,
  cleanCandidates,
  primaryHarness,
  buildActionPickerItems,
  buildManagerPickerItems,
  buildSlotPickerItems,
  actionSubjectFor,
  perfSnapshot,
  perfError,
}: {
  modal: Modal | null;
  current: WorktreeRow | undefined;
  /** Folded section under the cursor, when that's what `y` is yanking. */
  selectedSection: SelectedSection | undefined;
  rows: WorktreeRow[];
  cleanCandidates: readonly CleanCandidate[];
  primaryHarness: HarnessId;
  buildActionPickerItems: (target: WorktreeTarget) => PickerItem[];
  buildManagerPickerItems: (rowSlug: string | null) => PickerItem[];
  buildSlotPickerItems: () => PickerItem[];
  actionSubjectFor: ActionSubjectResolver;
  /** Undefined until the first sample lands (the overlay shows "sampling…"). */
  perfSnapshot: PerfSnapshot | undefined;
  /** Sampler failure, so a broken `ps` doesn't render as an idle machine. */
  perfError: Error | null;
}) {
  return (
    <>
      {modal?.kind === "help" && !process.env.WT_WORKSPACE_SOCKET ? (
        <HelpOverlay query={modal.query} searching={modal.searching} />
      ) : null}
      {modal?.kind === "perf" ? (
        <PerfOverlay
          snapshot={perfSnapshot}
          error={perfError}
          inject={modal.inject}
        />
      ) : null}
      {modal?.kind === "errors" ? <ErrorOverlay inject={modal.inject} /> : null}
      {modal?.kind === "devLogs" ? (
        <DevLogsOverlay slug={modal.slug} target={modal.target} />
      ) : null}
      {modal?.kind === "cleanConfirm" ? (
        <CleanConfirmModal candidates={cleanCandidates} />
      ) : null}
      {modal?.kind === "yank" && (selectedSection || current) ? (
        <YankModal
          items={selectedSection ? sectionYankItems(selectedSection) : yankItemsFor(current!)}
          selectedIndex={modal.index}
        />
      ) : null}
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
      {modal?.kind === "statusPicker" ? (
        <PickerModal
          title={`status for ${modal.slug}`}
          items={modal.items.map((it) => it.label)}
          selectedIndex={modal.index}
          toggleKey="u"
          itemKeys={modal.items.map((it) => it.chord)}
          // The same dot + color the list pane renders, so the picker
          // doubles as the legend for the status glyphs.
          itemGlyphs={modal.items.map((it) =>
            it.state === null
              ? null
              : {
                  glyph: workStateGlyph(it.glyphState ?? it.state),
                  color: workStateColor(it.glyphState ?? it.state),
                },
          )}
          extraHints={[["m", "pick + note"]]}
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
          surface={modal.state.surface}
          items={
            modal.state.surface === "manager"
              ? buildManagerPickerItems(modal.state.rowSlug)
              : modal.state.surface === "slot"
                ? buildSlotPickerItems()
                : buildActionPickerItems(modal.state.target!)
          }
          selectedIndex={modal.state.index}
        />
      ) : null}
      {modal?.kind === "actionPicker" && modal.state.mode === "edit" ? (
        <ActionEditModal
          slug={modal.state.slug}
          surface={modal.state.surface}
          def={modal.state.def}
          extras={modal.state.extras}
          vars={(() => {
            // Palette surfaces: fleet/slot builtins and custom text
            // render verbatim (no templates); the manager's row-scoped
            // entries preview against the row captured at open time —
            // the same row launchAction will render against.
            const st = modal.state;
            const prefix = actionSkillPrefix(st.def, primaryHarness);
            if (st.surface === "row" && st.target) {
              const subject = actionSubjectFor(st.target);
              return subject ? actionSubjectVars(subject, prefix) : {};
            }
            const row = st.surface === "manager" && st.rowSlug
              ? rows.find((candidate) => candidate.wt.slug === st.rowSlug)
              : undefined;
            return row ? buildActionVars(row, prefix) : {};
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
