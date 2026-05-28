import type { HistoryEntry } from "../../core/action-history.ts";
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

type ArgProps = {
  title: string;
  prompt: string;
  history: readonly HistoryEntry[];
  /**
   * Cursor index across `history.length + 1` rows — the trailing slot
   * is the "new value" affordance that opens the input on confirm.
   */
  index: number;
  /**
   * When non-null, the picker is in input mode (typing a fresh value).
   * The text field replaces the list footer.
   */
  input: string | null;
};

/**
 * Action-arg picker: shows recent values for one action with an
 * optional human label (sourced via the action's `label_extract` regex
 * against the run's captured output — see `core/action-history.ts`).
 * The trailing
 * row, "+ new value...", drops into a single-line input. Empty history
 * skips straight to input mode at the call site, so this list never
 * renders as just the "+ new" row alone.
 *
 * Modal UX rules: Enter / Esc only (no chord — reached mid-`!` flow,
 * see CLAUDE.md "When a picker doesn't naturally have a single trigger
 * key").
 */
export function ArgPickerModal({
  title,
  prompt,
  history,
  index,
  input,
}: ArgProps) {
  if (input !== null) {
    return (
      <Modal
        title={title}
        hints={[
          ["⏎", "launch"],
          ["esc", "back"],
        ]}
      >
        <box flexDirection="row" paddingLeft={1} paddingRight={1}>
          <text fg={theme.accent} attributes={1}>{prompt}</text>
          <text fg={theme.fg}> </text>
          <text fg={theme.fgBright}>{input}</text>
          <text fg={theme.accent}>█</text>
        </box>
      </Modal>
    );
  }
  const rows: Array<{ label: string; hint?: string; isNew: boolean }> = [];
  for (const entry of history) {
    rows.push({
      label: entry.label ?? entry.value,
      hint: entry.label ? entry.value : undefined,
      isNew: false,
    });
  }
  rows.push({ label: "+ new value…", isNew: true });
  return (
    <Modal
      title={title}
      hints={[
        ["j/k", "move"],
        ["⏎", "pick"],
        ["esc / q", "cancel"],
      ]}
    >
      {rows.map((row, i) => {
        const selected = i === index;
        const bg = selected ? theme.rowSelectedBg : undefined;
        const labelFg = row.isNew ? theme.accent : (selected ? theme.fgBright : theme.fg);
        return (
          <box
            key={`${i}-${row.label}`}
            flexDirection="row"
            backgroundColor={bg}
            paddingLeft={1}
            paddingRight={1}
          >
            <text fg={selected ? theme.accent : theme.fgDim}>
              {selected ? "▸ " : "  "}
            </text>
            <text fg={labelFg} wrapMode="none" truncate>
              {row.label}
              {row.hint ? <span fg={theme.fgDim}> · {row.hint}</span> : null}
            </text>
          </box>
        );
      })}
    </Modal>
  );
}
