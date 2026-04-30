import { Modal } from "../modal.tsx";
import { theme } from "../theme.ts";

type Props = {
  title: string;
  items: string[];
  selectedIndex: number;
};

export function PickerModal({ title, items, selectedIndex }: Props) {
  return (
    <Modal
      title={title}
      hints={[
        ["j/k", "move"],
        ["⏎", "pick"],
        ["esc / q", "cancel"],
      ]}
    >
      {items.map((item, i) => {
        const selected = i === selectedIndex;
        const bg = selected ? theme.rowSelectedBg : undefined;
        const fg = selected ? theme.fgBright : theme.fg;
        return (
          <box
            key={item}
            flexDirection="row"
            backgroundColor={bg}
            paddingLeft={1}
            paddingRight={1}
          >
            <text fg={selected ? theme.accent : theme.fgDim}>
              {selected ? "▸ " : "  "}
            </text>
            <text fg={fg} wrapMode="none" truncate>
              {item}
            </text>
          </box>
        );
      })}
    </Modal>
  );
}
