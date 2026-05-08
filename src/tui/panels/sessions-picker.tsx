/**
 * Sessions picker for `;` — lists live claude sessions on the current
 * row plus a "+ new" affordance. Two phases: a list view, and an
 * input view for typing a custom name when `+ new` is chosen.
 *
 * UX rules:
 *  - Primary always renders first when live.
 *  - Quick-pick digits track the rendered order.
 *  - `x` on a row kills that session without an extra confirm modal
 *    (live activity-pane log still records it). The user said kill
 *    is rare, so an extra keystroke matters more than a safety net.
 *  - Auto-name in the new-name input is the smallest unused integer
 *    starting at 2 (primary is implicit). Empty input → that name.
 */
import type { ClaudeSessionPickerEntry } from "../../core/claude-sessions.ts";
import { Modal } from "../modal.tsx";
import { theme } from "../theme.ts";

type ListProps = {
  slug: string;
  entries: ReadonlyArray<ClaudeSessionPickerEntry>;
  selectedIndex: number;
};

// React `key` for the sentinel "+ new" row appended after every
// entry list. Internal — callers identify "+ new" by its index
// (entries.length), not by this string.
const NEW_ROW_KEY = "__new__";

export function SessionsPickerList({
  slug,
  entries,
  selectedIndex,
}: ListProps) {
  const items: Array<{
    key: string;
    label: string;
    rightLabel: string;
    rightFg: string;
    labelFg: string;
  }> = [];
  for (const entry of entries) {
    const label = entry.name === null ? "primary" : entry.name;
    items.push({
      key: entry.name === null ? "primary" : `name:${entry.name}`,
      label,
      rightLabel: entry.isLive ? "live" : "ghost",
      rightFg: entry.isLive ? theme.accent : theme.fgDim,
      labelFg: entry.isLive ? theme.fg : theme.fgDim,
    });
  }
  items.push({
    key: NEW_ROW_KEY,
    label: "new session",
    rightLabel: "+",
    rightFg: theme.fgDim,
    labelFg: theme.fg,
  });
  return (
    <Modal
      title={`claude · ${slug}`}
      hints={[
        ["j/k", "move"],
        ["1-9", "quick pick"],
        ["⏎", "select"],
        ["x", "kill"],
        ["esc / q / ;", "cancel"],
      ]}
    >
      {items.map((it, i) => {
        const selected = i === selectedIndex;
        const bg = selected ? theme.rowSelectedBg : undefined;
        const fg = selected ? theme.fgBright : it.labelFg;
        const showDigit = i < 9;
        const prefix = showDigit ? `${i + 1}` : " ";
        return (
          <box
            key={it.key}
            flexDirection="row"
            backgroundColor={bg}
            paddingLeft={1}
            paddingRight={1}
          >
            <text fg={selected ? theme.accent : theme.fgDim}>
              {selected ? "▸ " : "  "}
            </text>
            <box width={2} flexShrink={0}>
              <text fg={theme.fgDim}>{prefix}</text>
            </box>
            <box flexGrow={1} flexShrink={1} overflow="hidden">
              <text fg={fg} wrapMode="none" truncate>
                {it.label}
              </text>
            </box>
            <text fg={it.rightFg}>{it.rightLabel}</text>
          </box>
        );
      })}
    </Modal>
  );
}

type NewProps = {
  slug: string;
  input: string;
  /**
   * Auto-name surfaced as the placeholder when input is empty —
   * what'll be used if the user just hits Enter. Computed by the
   * caller via `nextAutoName`.
   */
  autoName: string;
  /**
   * Validation error to display under the input, if any.
   */
  error: string | null;
};

export function SessionsPickerNew({ slug, input, autoName, error }: NewProps) {
  return (
    <Modal
      title={`claude · ${slug} · new`}
      hints={[
        ["⏎", "spawn & attach"],
        ["esc", "back"],
      ]}
    >
      <box flexDirection="row" paddingLeft={1} paddingRight={1}>
        <text fg={theme.fgDim}>name </text>
        <text fg={theme.accent}>{input || ""}</text>
        <text fg={theme.fgDim}>
          {input ? "" : `(blank → ${autoName})`}
        </text>
      </box>
      {error ? (
        <box paddingLeft={1} paddingRight={1}>
          <text fg={theme.err}>{error}</text>
        </box>
      ) : null}
    </Modal>
  );
}
