import { Modal } from "../modal.tsx";
import { theme } from "../theme.ts";

type Item = { key: string; label: string; hint: string };

/**
 * Stack chord menu. Sync fetches origin and rebases every chain on
 * trunk; rebase touches just the chain containing the current row.
 * Both go through the same executor and escalate to a claude session
 * on the first rebase conflict.
 */
export const STACK_ITEMS: Item[] = [
  {
    key: "s",
    label: "sync",
    hint: "fetch origin, rebase every chain on trunk, force-push",
  },
  {
    key: "r",
    label: "rebase",
    hint: "rebase the current chain on its parents, force-push",
  },
  {
    key: "p",
    label: "set base",
    hint: "manually set this worktree's parent branch",
  },
];

export function StackActionsModal() {
  return (
    <Modal
      title="stack · pick action"
      inset={{ top: "35%", right: "20%", bottom: "40%", left: "20%" }}
      hints={[["esc / q / b", "cancel"]]}
    >
      {STACK_ITEMS.map((it) => (
        <box key={it.key} flexDirection="row">
          <box width={3} flexShrink={0}>
            <text fg={theme.accent} attributes={1}>
              {it.key}
            </text>
          </box>
          <box width={10} flexShrink={0}>
            <text fg={theme.fg}>{it.label}</text>
          </box>
          <box flexShrink={1} overflow="hidden">
            <text fg={theme.fgDim} wrapMode="none" truncate>
              {it.hint}
            </text>
          </box>
        </box>
      ))}
    </Modal>
  );
}
