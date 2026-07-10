import { Modal } from "../modal.tsx";
import { theme } from "../theme.ts";

type Props = {
  title: string;
  /** Primary one-line question (no trailing "[y/N]"; the hints carry it). */
  message: string;
  /** Optional dim second line for stakes/context (e.g. what gets lost). */
  detail?: string;
  /** Verb shown next to `y / ⏎` in the hints; defaults to "confirm". */
  confirmLabel?: string;
  /** Destructive actions render a warn border; otherwise accent. */
  danger?: boolean;
};

/**
 * Generic y/N confirmation. Lives in the modal layer (not the footer) so
 * an async toast — e.g. a delayed "1 reviewer set" landing while the user
 * lines up `y` — can't clobber the pending prompt the way it did when
 * confirms shared the footer's single state slot. Driven by the
 * `{ kind: "confirm" }` modal (mounted in `modal-host.tsx`); `modal-keys/confirm.ts` dispatches on `pendingKey`.
 */
export function ConfirmModal({
  title,
  message,
  detail,
  confirmLabel = "confirm",
  danger,
}: Props) {
  return (
    <Modal
      title={title}
      borderColor={danger ? theme.warn : theme.accent}
      inset={{ top: "30%", right: "25%", bottom: "30%", left: "25%" }}
      hints={[
        ["y / ⏎", confirmLabel],
        ["n / esc / q", "cancel"],
      ]}
    >
      <box flexDirection="column">
        <text fg={theme.fg} wrapMode="word">
          {message}
        </text>
        {detail ? (
          <box marginTop={1} flexDirection="column">
            <text fg={theme.fgDim} wrapMode="word">
              {detail}
            </text>
          </box>
        ) : null}
      </box>
    </Modal>
  );
}
