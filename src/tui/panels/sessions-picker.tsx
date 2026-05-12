/**
 * Sessions picker for `;` — lists every claude session known to wt for
 * the current worktree plus a "+ new" affordance. Two phases: a list
 * view, and an input view for typing a custom name when `+ new` is
 * chosen.
 *
 * UX rules:
 *  - Sort is status-priority-first (working > waiting > abandoned >
 *    idle), then primary > named, then most-recent-active first. The
 *    user's "what should I look at" eye lands on row 1 every time.
 *  - Status drives color (accent / warn / err / dim) for the right-side
 *    label, matching the details-pane claude row.
 *  - The summary panel below the list shows the LLM-authored snippet
 *    for the selected entry (ai-title > away_summary > last-prompt
 *    fallback). Modal height is fixed so navigation doesn't reflow.
 *  - Quick-pick digits track the rendered (sorted) order.
 *  - `x` on a row kills that session without an extra confirm modal.
 *  - Auto-name on `+ new` is the smallest unused integer starting at 2
 *    (primary is implicit). Empty input → that name.
 *  - Live-preview / pin: j/k on a live entry points the bottom pane
 *    at that session's log, mirroring the outputs picker. `'` pins or
 *    unpins, same shape as `OutputsPicker`.
 */
import { TextAttributes } from "@opentui/core";

import type { ClaudeSessionPickerEntry } from "../../core/claude-sessions.ts";
import type { DerivedState } from "../../core/claude-status.ts";
import { sessionOutputId } from "../../core/outputs.ts";
import { STATE_DOT, STATE_FG } from "../claude-state.ts";
import { NF } from "../icons.ts";
import { Modal } from "../modal.tsx";
import { ageMsToText } from "../text.ts";
import { theme } from "../theme.ts";

type ListProps = {
  slug: string;
  entries: ReadonlyArray<ClaudeSessionPickerEntry>;
  selectedIndex: number;
  /**
   * Currently pinned output id for this slug's bucket, or null when
   * nothing is pinned. Used to render the pin glyph next to the
   * matching entry; also drives the hint label (`'` toggles).
   */
  pinnedId: string | null;
};

// React `key` for the sentinel "+ new" row appended after every
// entry list. Internal — callers identify "+ new" by its index
// (entries.length), not by this string.
const NEW_ROW_KEY = "__new__";

/**
 * Render the LLM-authored summary blob for the selected entry.
 * Italicized so the prose reads as "context about", not "row content"
 * — the body is model output, not a label.
 */
function SummaryPanel({
  entry,
}: {
  entry: ClaudeSessionPickerEntry | null;
}) {
  if (!entry) {
    return (
      <text fg={theme.fgDim} attributes={TextAttributes.ITALIC} wrapMode="word">
        no session selected
      </text>
    );
  }
  const summary = entry.summary;
  if (!summary) {
    return (
      <text fg={theme.fgDim} attributes={TextAttributes.ITALIC} wrapMode="word">
        (no summary yet)
      </text>
    );
  }
  return (
    <text fg={theme.fg} attributes={TextAttributes.ITALIC} wrapMode="word">
      {summary.text}
    </text>
  );
}

export function SessionsPickerList({
  slug,
  entries,
  selectedIndex,
  pinnedId,
}: ListProps) {
  const items: Array<{
    key: string;
    label: string;
    state: DerivedState | null;
    statusText: string;
    ageText: string | null;
    queued: number;
    isPinned: boolean;
  }> = [];
  for (const entry of entries) {
    const label = entry.name === null ? "primary" : entry.name;
    const outputId = sessionOutputId(slug, "claude", entry.name);
    const ageText =
      entry.lastEntryMs !== null
        ? ageMsToText(Date.now() - entry.lastEntryMs)
        : null;
    items.push({
      key: entry.name === null ? "primary" : `name:${entry.name}`,
      label,
      state: entry.state,
      statusText: entry.state,
      ageText,
      queued: entry.queued,
      isPinned: pinnedId === outputId,
    });
  }
  items.push({
    key: NEW_ROW_KEY,
    label: "new session",
    state: null,
    statusText: "+",
    ageText: null,
    queued: 0,
    isPinned: false,
  });

  const selectedEntry =
    selectedIndex < entries.length ? entries[selectedIndex] ?? null : null;

  return (
    <Modal
      title={`claude · ${slug}`}
      // Bigger than the default 20% so the summary panel below the
      // list has room to breathe. Consistent height across selections
      // keeps j/k navigation steady — no reflow on summary swap.
      inset={{ top: "8%", bottom: "8%" }}
      hints={[
        ["j/k", "move"],
        ["1-9", "quick pick"],
        ["⏎", "select"],
        ["'", pinnedId ? "unpin" : "pin"],
        ["x", "kill"],
        ["esc / q / ;", "cancel"],
      ]}
    >
      <box flexDirection="column" flexGrow={1} overflow="hidden">
        {items.map((it, i) => {
          const selected = i === selectedIndex;
          const bg = selected ? theme.rowSelectedBg : undefined;
          // Label fg: bright when selected, state-tinted dim when
          // not — keeps the eye flowing down the active set without
          // making non-selected rows feel washed out.
          const labelFg = selected
            ? theme.fgBright
            : it.state === null
              ? theme.fg
              : it.state === "idle"
                ? theme.fgDim
                : theme.fg;
          const stateFg = it.state ? STATE_FG[it.state] : theme.fgDim;
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
                <text fg={labelFg} wrapMode="none" truncate>
                  {it.label}
                </text>
              </box>
              {it.queued > 0 ? (
                <text fg={theme.warn}>{it.queued}⏵ </text>
              ) : null}
              {it.isPinned ? <text fg={theme.accent}>{NF.pin} </text> : null}
              {it.state ? (
                <text fg={stateFg}>
                  {STATE_DOT[it.state]} {it.statusText}
                </text>
              ) : (
                <text fg={stateFg}>{it.statusText}</text>
              )}
              {it.ageText ? (
                <text fg={theme.fgDim}> · {it.ageText}</text>
              ) : null}
            </box>
          );
        })}
        <box flexShrink={0} marginTop={1} flexDirection="row">
          <text fg={theme.fgDim}>
            {"─".repeat(2)} summary {"─".repeat(2)}
          </text>
        </box>
        <box
          flexGrow={1}
          flexShrink={1}
          overflow="hidden"
          paddingLeft={1}
          paddingRight={1}
          marginTop={1}
        >
          <SummaryPanel entry={selectedEntry} />
        </box>
      </box>
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
