import {
  type Output,
  type OutputStatus,
  outputStatusLabel,
} from "../../core/outputs.ts";
import { Modal } from "../modal.tsx";
import { NF } from "../icons.ts";
import { theme } from "../theme.ts";

function statusFg(status: OutputStatus): string {
  switch (status) {
    case "running":
    case "live":
      return theme.accent;
    case "done":
      return theme.ok;
    case "failed":
      return theme.err;
    case "killed":
      return theme.warn;
  }
}

function relTime(ts: number, now: number): string {
  const diff = Math.max(0, now - ts);
  if (diff < 60_000) return `${Math.floor(diff / 1000)}s ago`;
  if (diff < 3_600_000) return `${Math.floor(diff / 60_000)}m ago`;
  if (diff < 86_400_000) return `${Math.floor(diff / 3_600_000)}h ago`;
  return `${Math.floor(diff / 86_400_000)}d ago`;
}

type Props = {
  /**
   * Slug whose outputs are listed, or `null` for the no-row scope
   * (events only). Surfaces in the modal title so the user knows
   * which worktree's pin/focus they're editing — buckets are
   * per-worktree.
   */
  slug: string | null;
  items: readonly Output[];
  selectedIndex: number;
  /** Id of the pinned output, if any — drives the row pin glyph. */
  pinnedId: string | null;
};

export function OutputsPicker({
  slug,
  items,
  selectedIndex,
  pinnedId,
}: Props) {
  const now = Date.now();
  const hints: Array<[string, string]> = [
    ["j/k", "move"],
    ["1-9", "quick pick"],
    ["⏎", "select"],
    ["*", pinnedId ? "unpin" : "pin"],
    ["esc / q", "cancel"],
  ];
  // "(no row)" rather than "global" when nothing is selected: the
  // outputs themselves haven't changed scope, only the bucket the
  // user's pin/focus lands in. "global" implied per-session-wide
  // outputs which isn't what's happening.
  const title = slug ? `outputs · ${slug}` : "outputs · (no row)";
  return (
    <Modal title={title} hints={hints}>
      {items.length === 0 ? (
        <text fg={theme.fgDim}>(no outputs)</text>
      ) : (
        items.map((o, i) => {
          const selected = i === selectedIndex;
          const isPinned = pinnedId === o.id;
          const bg = selected ? theme.rowSelectedBg : undefined;
          const fg = selected ? theme.fgBright : theme.fg;
          const showDigit = i < 9;
          const prefix = showDigit ? `${i + 1}` : " ";
          const isLive = o.status === "running" || o.status === "live";
          const right =
            o.kind === "events"
              ? "global"
              : isLive
                ? "·"
                : relTime(o.lastActivity, now);
          return (
            <box
              key={o.id}
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
              <box width={9} flexShrink={0}>
                <text fg={statusFg(o.status)}>{outputStatusLabel(o.status)}</text>
              </box>
              <box flexGrow={1} flexShrink={1} overflow="hidden">
                <text fg={fg} wrapMode="none" truncate>
                  {o.title}
                </text>
              </box>
              {/*
                Pin glyph at the right edge so the quick-pick digit
                in the prefix slot stays visible — pressing `1` to
                jump to the pinned row should still work, and a user
                scanning by digit shouldn't see one row's number
                replaced by an icon.
              */}
              {isPinned ? (
                <text fg={theme.accent}>{NF.pin} </text>
              ) : null}
              <text fg={theme.fgDim}>{right}</text>
            </box>
          );
        })
      )}
    </Modal>
  );
}
