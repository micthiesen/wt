import { Modal } from "../modal.tsx";
import { theme } from "../theme.ts";

type Props = {
  slug: string;
  actionName: string;
};

export function KillActionConfirmModal({ slug, actionName }: Props) {
  return (
    <Modal
      title="kill action"
      borderColor={theme.warn}
      inset={{ top: "30%", right: "25%", bottom: "30%", left: "25%" }}
      hints={[
        ["y", "kill"],
        ["! / n / esc / q", "cancel"],
      ]}
    >
      <box flexDirection="column">
        <text fg={theme.fg}>
          Kill{" "}
          <span fg={theme.warn} attributes={1}>
            {actionName}
          </span>{" "}
          on{" "}
          <span fg={theme.accent}>{slug}</span>
          ?
        </text>
        <box marginTop={1} flexDirection="column">
          <text fg={theme.fgDim} wrapMode="word">
            The Claude process gets SIGTERM. Any in-progress git/SST
            commands it spawned can keep running until they finish.
          </text>
        </box>
      </box>
    </Modal>
  );
}
