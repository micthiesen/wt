import { Modal } from "../modal.tsx";
import { theme } from "../theme.ts";

type Props = {
  slug: string;
};

export function KillSessionConfirmModal({ slug }: Props) {
  return (
    <Modal
      title="kill session"
      borderColor={theme.warn}
      inset={{ top: "30%", right: "25%", bottom: "30%", left: "25%" }}
      hints={[
        ["y", "kill"],
        ["n / esc / q", "cancel"],
      ]}
    >
      <box flexDirection="column">
        <text fg={theme.fg}>
          Kill the interactive Claude session on{" "}
          <span fg={theme.accent}>{slug}</span>
          ?
        </text>
        <box marginTop={1} flexDirection="column">
          <text fg={theme.fgDim} wrapMode="word">
            The tmux session and the running Claude process are
            terminated. Conversation history is preserved on disk —
            next F12 resumes the same conversation. Use /clear inside
            Claude if you want a fresh context.
          </text>
        </box>
      </box>
    </Modal>
  );
}
