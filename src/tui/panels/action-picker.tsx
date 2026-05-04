import type { ActionDef } from "../../core/config.ts";
import { Modal } from "../modal.tsx";
import { theme } from "../theme.ts";

/**
 * Picker-mode item: one of the configured actions, or the trailing
 * "Custom prompt..." entry that drops you straight into a freeform
 * editor with no template prefix.
 */
export type PickerItem =
  | { kind: "action"; def: ActionDef }
  | { kind: "custom" };

/**
 * Two-screen state machine. Esc in `edit` pops back to `list` when a
 * pre-built was selected (informative restore point) or cancels out
 * entirely from custom (no list state worth restoring).
 */
export type ActionPickerState =
  | { mode: "list"; slug: string; index: number; items: PickerItem[] }
  | { mode: "edit"; slug: string; def: ActionDef | null; extras: string };

type Props = {
  slug: string;
  items: PickerItem[];
  selectedIndex: number;
};

export function ActionPickerModal({ slug, items, selectedIndex }: Props) {
  return (
    <Modal
      title={`action · ${slug}`}
      hints={[
        ["j/k", "move"],
        ["⏎", "select"],
        ["! / esc / q", "cancel"],
      ]}
    >
      {items.map((item, i) => {
        const selected = i === selectedIndex;
        const bg = selected ? theme.rowSelectedBg : undefined;
        const fg = selected ? theme.fgBright : theme.fg;
        const label =
          item.kind === "custom" ? "Custom prompt…" : item.def.name;
        const hint =
          item.kind === "custom"
            ? "freeform"
            : item.def.id;
        return (
          <box
            key={item.kind === "custom" ? "__custom__" : item.def.id}
            flexDirection="row"
            backgroundColor={bg}
            paddingLeft={1}
            paddingRight={1}
          >
            <text fg={selected ? theme.accent : theme.fgDim}>
              {selected ? "▸ " : "  "}
            </text>
            <box flexGrow={1} flexShrink={1} overflow="hidden">
              <text fg={fg} wrapMode="none" truncate>
                {label}
              </text>
            </box>
            <text fg={theme.fgDim}>{hint}</text>
          </box>
        );
      })}
    </Modal>
  );
}

type EditProps = {
  slug: string;
  /** `null` = custom prompt (extras IS the entire prompt). */
  def: ActionDef | null;
  extras: string;
};

export function ActionEditModal({ slug, def, extras }: EditProps) {
  const title = def ? `action · ${def.name} · ${slug}` : `action · custom · ${slug}`;
  return (
    <Modal
      title={title}
      hints={[
        ["⏎", "launch"],
        ["esc", def ? "back" : "cancel"],
        ["^C", "cancel"],
      ]}
    >
      {def ? (
        <box flexDirection="column" marginBottom={1}>
          <text fg={theme.fgDim} attributes={1}>
            prompt
          </text>
          <box flexDirection="column" marginTop={0}>
            {def.prompt.split("\n").map((line, i) => (
              <text key={i} fg={theme.fg} wrapMode="word">
                {line || " "}
              </text>
            ))}
          </box>
        </box>
      ) : null}
      <box flexDirection="column" flexGrow={1}>
        <text fg={theme.fgDim} attributes={1}>
          {def ? "additional instructions" : "prompt"}
        </text>
        <box flexDirection="column" marginTop={0} flexGrow={1}>
          {extras.length === 0 ? (
            <text fg={theme.fgDim}>
              <span fg={theme.accent}>█</span>
              {def
                ? " (optional — type to append, ⏎ to launch)"
                : " (type your prompt, ⏎ to launch)"}
            </text>
          ) : (
            <ExtrasView text={extras} />
          )}
        </box>
      </box>
    </Modal>
  );
}

function ExtrasView({ text }: { text: string }) {
  const lines = text.split("\n");
  return (
    <>
      {lines.map((line, i) => {
        const isLast = i === lines.length - 1;
        return (
          <text key={i} fg={theme.fgBright} wrapMode="word">
            {line}
            {isLast ? <span fg={theme.accent}>█</span> : null}
          </text>
        );
      })}
    </>
  );
}
