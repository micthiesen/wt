import { Modal } from "../modal.tsx";
import { theme } from "../theme.ts";

export type SectionPickerItem =
  | { kind: "none" }
  | { kind: "section"; name: string }
  | { kind: "create" }
  | {
      kind: "stack";
      /** Whether picking this entry creates or removes the stack section. */
      mode: "create" | "remove";
      /** Section name to register or drop. */
      name: string;
      /** Root slug of the chain — recorded into sectionMeta on create. */
      rootSlug: string;
    };

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
  if (item.kind === "none") return "(none)";
  if (item.kind === "create") return "+ new section";
  if (item.kind === "stack") {
    return item.mode === "create"
      ? `+ stack section (${item.name})`
      : `× remove ${item.name}`;
  }
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
        ["l", "new section"],
        ["⏎", "select"],
        ["esc / q", "cancel"],
      ]}
    >
      {items.map((item, i) => {
        const selected = i === selectedIndex;
        const bg = selected ? theme.rowSelectedBg : undefined;
        const fg = selected ? theme.fgBright : theme.fg;
        // Quick-pick digit prefix for the first 9 items (1..9). The
        // create entry shows "l" instead — matches the chord shortcut
        // (`l l` from normal mode jumps straight into create-name).
        const isCreate = item.kind === "create";
        const isStack = item.kind === "stack";
        const showDigit = i < 9 && !isCreate && !isStack;
        const prefix = isCreate
          ? "l"
          : isStack
            ? "├"
            : showDigit
              ? `${i + 1}`
              : " ";
        const prefixFg = isCreate
          ? theme.accent
          : isStack
            ? theme.accentAlt
            : theme.fgDim;
        const labelFg = isCreate
          ? theme.accent
          : isStack
            ? theme.accentAlt
            : item.kind === "none"
              ? theme.fgDim
              : fg;
        return (
          <box
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
    </Modal>
  );
}
