import { Modal } from "../modal.tsx";
import { theme } from "../theme.ts";

type Props = {
  slug: string;
  /** Which session kind is being killed — drives the body copy. */
  sessionKind: "claude" | "diff";
};

const COPY: Record<Props["sessionKind"], { title: string; body: string }> = {
  claude: {
    title: "Kill the interactive Claude session on",
    body:
      "The tmux session and the running Claude process are " +
      "terminated. Conversation history is preserved on disk — " +
      "next F12 resumes the same conversation. Use /clear inside " +
      "Claude if you want a fresh context.",
  },
  diff: {
    title: "Kill the diff session on",
    body:
      "The tmux session and the diff TUI are terminated. Next F11 " +
      "opens a fresh session — scroll position and expanded hunks " +
      "won't carry over.",
  },
};

export function KillSessionConfirmModal({ slug, sessionKind }: Props) {
  const copy = COPY[sessionKind];
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
          {copy.title} <span fg={theme.accent}>{slug}</span>
          ?
        </text>
        <box marginTop={1} flexDirection="column">
          <text fg={theme.fgDim} wrapMode="word">
            {copy.body}
          </text>
        </box>
      </box>
    </Modal>
  );
}
