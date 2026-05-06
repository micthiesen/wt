import { applyVars, type ActionVars } from "../../core/actions.ts";
import type { ActionDef } from "../../core/config.ts";
import { Modal } from "../modal.tsx";
import { theme } from "../theme.ts";

/** Claude-flavored action def — the only kind that uses the edit modal. */
type ClaudeActionDef = Extract<ActionDef, { kind: "claude" }>;

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
 * entirely from custom (no list state worth restoring). Only claude-
 * flavored actions reach `edit`; shell actions launch directly from
 * `list`.
 */
export type ActionPickerState =
  | { mode: "list"; slug: string; index: number; items: PickerItem[] }
  | { mode: "edit"; slug: string; def: ClaudeActionDef | null; extras: string };

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
        ["1-9", "quick pick"],
        ["!", "custom prompt"],
        ["⏎", "select"],
        ["esc / q", "cancel"],
      ]}
    >
      {items.map((item, i) => {
        const selected = i === selectedIndex;
        const bg = selected ? theme.rowSelectedBg : undefined;
        const fg = selected ? theme.fgBright : theme.fg;
        // Custom entry gets the `!` chord prefix (mirrors `l` for "+ new
        // section"); configured actions get 1..9 quick-pick digits.
        const isCustom = item.kind === "custom";
        const actionIndex = isCustom ? -1 : i;
        const showDigit = !isCustom && actionIndex < 9;
        const prefix = isCustom ? "!" : showDigit ? `${actionIndex + 1}` : " ";
        const prefixFg = isCustom ? theme.accent : theme.fgDim;
        const labelFg = isCustom ? theme.accent : fg;
        const label = isCustom ? "Custom prompt…" : item.def.name;
        const hint = isCustom
          ? "freeform"
          : item.def.kind === "shell"
            ? `$ ${item.def.id}`
            : item.def.id;
        return (
          <box
            key={isCustom ? "__custom__" : item.def.id}
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
            <box flexGrow={1} flexShrink={1} overflow="hidden">
              <text fg={labelFg} wrapMode="none" truncate>
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
  def: ClaudeActionDef | null;
  extras: string;
  /**
   * Substitutions for `{{name}}` in `def.prompt`. Mirrors what gets
   * applied at launch, so the preview matches what claude actually
   * receives.
   */
  vars: ActionVars;
};

export function ActionEditModal({ slug, def, extras, vars }: EditProps) {
  const title = def ? `action · ${def.name} · ${slug}` : `action · custom · ${slug}`;
  const renderedPrompt = def ? applyVars(def.prompt, vars) : "";
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
            {renderedPrompt.split("\n").map((line, i) => (
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
