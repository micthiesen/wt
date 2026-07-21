import { Modal } from "../modal.tsx";
import { ScrollableList } from "./scroll-list.tsx";
import { theme } from "../theme.ts";

export type SectionPickerItem =
  | { kind: "none" }
  | { kind: "section"; name: string }
  | { kind: "create" };

type Props = {
  title: string;
  items: SectionPickerItem[];
  selectedIndex: number;
  /**
   * When non-null we're in "+ new section" input mode — the modal
   * shows the input prompt instead of the list. Enter commits the
   * typed name, esc returns to the picker.
   */
  newName: string | null;
};

function itemLabel(item: SectionPickerItem): string {
  if (item.kind === "none") return "Inbox";
  if (item.kind === "create") return "+ new section";
  return item.name;
}

export function SectionPickerModal({ title, items, selectedIndex, newName }: Props) {
  if (newName !== null) {
    return (
      <Modal
        title={title}
        hints={[
          ["⏎", "create"],
          ["esc", "back"],
        ]}
      >
        <box flexDirection="row" paddingLeft={1}>
          <text fg={theme.fgDim}>name: </text>
          <text fg={theme.fgBright}>{newName}</text>
          <text fg={theme.accent}>▎</text>
        </box>
      </Modal>
    );
  }
  return (
    <Modal
      title={title}
      hints={[
        ["j/k", "move"],
        ["1-9", "quick pick"],
        ["n", "new section"],
        ["l / ⏎", "select"],
        ["esc / q", "cancel"],
      ]}
    >
      <ScrollableList selectedId={`sec:${selectedIndex}`} revision={items}>
      {items.map((item, i) => {
        const selected = i === selectedIndex;
        const bg = selected ? theme.rowSelectedBg : undefined;
        const fg = selected ? theme.fgBright : theme.fg;
        // Quick-pick digit prefix for the first 9 items (1..9). The
        // create entry shows "n" instead — matches the chord shortcut
        // (`l n` from normal mode jumps straight into create-name).
        const isCreate = item.kind === "create";
        const showDigit = i < 9 && !isCreate;
        const prefix = isCreate ? "n" : showDigit ? `${i + 1}` : " ";
        const prefixFg = isCreate ? theme.accent : theme.fgDim;
        const labelFg = isCreate
          ? theme.accent
          : item.kind === "none"
            ? theme.fgDim
            : fg;
        return (
          <box
            id={`sec:${i}`}
            key={i}
            flexDirection="row"
            backgroundColor={bg}
            paddingLeft={1}
            paddingRight={1}
          >
            <text fg={selected ? theme.accent : theme.fgDim}>
              {selected ? "▸ " : "  "}
            </text>
            <box width={2} flexShrink={0}>
              <text fg={prefixFg}>{prefix}</text>
            </box>
            <text fg={labelFg} wrapMode="none" truncate>
              {itemLabel(item)}
            </text>
          </box>
        );
      })}
      </ScrollableList>
    </Modal>
  );
}
