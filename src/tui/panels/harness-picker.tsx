/**
 * Pick-an-AI-harness modal. Opened by Shift+F12 as a one-off override
 * of the primary harness for the next spawn. Mirrors the trigger-key
 * confirm + j/k + digits + per-harness letter shortcut pattern shared
 * across every list-picker in the TUI.
 *
 * Per-harness letters come from each impl's `letter` field — `c` for
 * Claude, `o` for OpenCode, `x` for Codex. Pressing the letter jumps
 * the highlight to that row; F12 / Enter then confirms.
 */
import { HARNESSES } from "../../core/harness/index.ts";
import { Modal } from "../modal.tsx";
import { theme } from "../theme.ts";

type Props = {
  slug: string;
  selectedIndex: number;
};

export function HarnessPickerModal({ slug, selectedIndex }: Props) {
  const items = HARNESSES;
  return (
    <Modal
      title={`pick harness · ${slug}`}
      inset={{ top: "30%", right: "30%", bottom: "30%", left: "30%" }}
      hints={[
        ["j/k", "move"],
        ["c / o / x", "jump"],
        ["F12 / ⏎", "spawn"],
        ["esc / q", "cancel"],
      ]}
    >
      <box flexDirection="column" flexGrow={1}>
        {items.map((h, i) => {
          const selected = i === selectedIndex;
          const bg = selected ? theme.rowSelectedBg : undefined;
          return (
            <box
              key={h.id}
              flexDirection="row"
              backgroundColor={bg}
              paddingLeft={1}
              paddingRight={1}
            >
              <text fg={selected ? theme.accent : theme.fgDim}>
                {selected ? "▸ " : "  "}
              </text>
              <box width={2} flexShrink={0}>
                <text fg={theme.fgDim}>{h.letter}</text>
              </box>
              <text fg={h.color}>{h.glyph} </text>
              <text fg={selected ? theme.fgBright : theme.fg}>{h.label}</text>
            </box>
          );
        })}
      </box>
    </Modal>
  );
}
