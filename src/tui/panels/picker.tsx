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

export type MultiPickerItem = {
  /** Stable identity used for selection set membership. */
  key: string;
  /** Primary text shown for the item. */
  label: string;
  /** Optional dim suffix (e.g. "(requested)"). */
  hint?: string;
};

type MultiProps = {
  title: string;
  items: MultiPickerItem[];
  selectedIndex: number;
  checked: ReadonlySet<string>;
  /**
   * Key that opens & closes the modal. When set, it's prepended to
   * the cancel hint so the user sees the full dismiss vocabulary
   * (toggle key + universal esc/q). See `Modal` for the convention.
   */
  toggleKey?: string;
};

export function MultiPickerModal({
  title,
  items,
  selectedIndex,
  checked,
  toggleKey,
}: MultiProps) {
  const cancelKeys = toggleKey ? `${toggleKey} / esc / q` : "esc / q";
  return (
    <Modal
      title={title}
      hints={[
        ["j/k", "move"],
        ["space", "toggle"],
        ["⏎", "submit"],
        [cancelKeys, "cancel"],
      ]}
    >
      {items.length === 0 ? (
        <text fg={theme.fgDim}>no candidates</text>
      ) : null}
      {items.map((item, i) => {
        const cursor = i === selectedIndex;
        const isChecked = checked.has(item.key);
        const bg = cursor ? theme.rowSelectedBg : undefined;
        const fg = cursor ? theme.fgBright : theme.fg;
        const box = isChecked ? "[x]" : "[ ]";
        const boxFg = isChecked ? theme.ok : theme.fgDim;
        return (
          <box
            key={item.key}
            flexDirection="row"
            backgroundColor={bg}
            paddingLeft={1}
            paddingRight={1}
          >
            <text fg={cursor ? theme.accent : theme.fgDim}>
              {cursor ? "▸ " : "  "}
            </text>
            <text fg={boxFg}>{box} </text>
            <text fg={fg} wrapMode="none" truncate>
              {item.label}
              {item.hint ? (
                <span fg={theme.fgDim}> {item.hint}</span>
              ) : null}
            </text>
          </box>
        );
      })}
    </Modal>
  );
}
