import { Modal } from "../modal.tsx";
import { theme } from "../theme.ts";

type Props = {
  title: string;
  items: string[];
  selectedIndex: number;
  /**
   * Key that opens the modal — re-pressing it confirms (chord-confirm
   * convention; see modal UX rules in CLAUDE.md). Shown in the "pick"
   * hint when set.
   */
  toggleKey?: string;
};

export function PickerModal({
  title,
  items,
  selectedIndex,
  toggleKey,
}: Props) {
  const pickKeys = toggleKey ? `${toggleKey} / ⏎` : "⏎";
  return (
    <Modal
      title={title}
      hints={[
        ["j/k", "move"],
        [pickKeys, "pick"],
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
   * Key that opens the modal — re-pressing it submits (chord-confirm
   * convention; see modal UX rules in CLAUDE.md). Shown in the
   * "submit" hint when set.
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
  const submitKeys = toggleKey ? `${toggleKey} / ⏎` : "⏎";
  return (
    <Modal
      title={title}
      hints={[
        ["j/k", "move"],
        ["space", "toggle"],
        [submitKeys, "submit"],
        ["esc / q", "cancel"],
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
