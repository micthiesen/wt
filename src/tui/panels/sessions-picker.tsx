/**
 * Sessions picker for `;` — multi-harness list of every session known
 * to wt for the current worktree plus a per-harness "+ new" affordance.
 * Two phases: a list view, and an input view for typing a custom name
 * when "+ new claude" is chosen (codex / opencode generate their own
 * session ids so they skip the name-input phase).
 *
 * UX rules:
 *  - Sort: live sessions first (most-recently-active inside each
 *    harness's live set wins), then dead sessions (by recency across
 *    harnesses). Heterogeneous status (claude has busy/idle, codex /
 *    opencode don't) makes a strict state-priority sort impossible
 *    across the whole list — the dead-vs-live split is the most
 *    legible thing left.
 *  - Status color drives the right-side label. Claude entries surface
 *    their derived state (working / waiting / abandoned / idle) via
 *    the per-state color; codex / opencode show a simple "live" or
 *    age glyph in dim.
 *  - The summary panel below the list shows the LLM-authored snippet
 *    for the selected entry when one exists. Today only Claude
 *    supplies summaries — codex / opencode entries fall back to
 *    "(no summary yet)".
 *  - Quick-pick digits track the rendered order of the SESSION rows
 *    only; "+ new" rows are reached via per-harness letters.
 *  - `d` on a row kills that session (claude only; for codex /
 *    opencode the picker shows a hint and the user kills the tmux
 *    slot from outside).
 *  - Live-preview: j/k on a live claude entry points the bottom pane
 *    at that session's log, mirroring the outputs picker.
 */
import { TextAttributes } from "@opentui/core";

import type { DerivedState } from "../../core/claude-status.ts";
import { HARNESSES, type HarnessId } from "../../core/harness/index.ts";
import { STATE_DOT, STATE_FG } from "../claude-state.ts";
import type { HarnessSessionEntry } from "../hooks/useHarnessSessions.ts";
import { Modal } from "../modal.tsx";
import { ageMsToText } from "../text.ts";
import { theme } from "../theme.ts";

export type PickerRow =
  | { kind: "session"; entry: HarnessSessionEntry }
  | { kind: "new"; harnessId: HarnessId };

type SummaryBySessionId = ReadonlyMap<string, { text: string } | null>;

type ListProps = {
  slug: string;
  rows: ReadonlyArray<PickerRow>;
  selectedIndex: number;
  /**
   * Per-claude-session summary snippets (ai-title / away_summary /
   * last-prompt). Keyed by session UUID; passed through from the
   * existing `claudeSummariesQuery`. Codex / opencode entries don't
   * have summaries today.
   */
  summaries: SummaryBySessionId;
};

/**
 * Render the LLM-authored summary blob for the selected entry. Empty
 * for codex / opencode (no summary source yet). Italicized so the
 * prose reads as "context about", not "row content".
 */
function SummaryPanel({
  row,
  summaries,
}: {
  row: PickerRow | null;
  summaries: SummaryBySessionId;
}) {
  if (!row || row.kind !== "session") {
    return (
      <text fg={theme.fgDim} attributes={TextAttributes.ITALIC} wrapMode="word">
        no session selected
      </text>
    );
  }
  const summary = summaries.get(row.entry.sessionId);
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
  rows,
  selectedIndex,
  summaries,
}: ListProps) {
  // Track digit assignments for session rows only; "+ new" rows get
  // their per-harness letter prefix instead.
  let sessionDigitCursor = 0;
  // Detect the boundary between session rows and "+ new" rows so we
  // can insert a spacer between them.
  const firstNewIndex = rows.findIndex((r) => r.kind === "new");
  return (
    <Modal
      title={`sessions · ${slug}`}
      inset={{ top: "8%", bottom: "8%" }}
      hints={[
        ["j/k ↑↓", "move"],
        ["1-9", "quick pick"],
        // Letter order follows HARNESSES registry order (which also
        // controls the rendered row order for the "+ new" affordances)
        // so the hint reads top-to-bottom.
        [HARNESSES.map((h) => h.letter).join(" / "), "new …"],
        ["d", "kill"],
        ["; / ⏎", "select"],
        ["esc / q", "cancel"],
      ]}
    >
      <box flexDirection="column" flexGrow={1} overflow="hidden">
        {rows.map((row, i) => {
          const selected = i === selectedIndex;
          const bg = selected ? theme.rowSelectedBg : undefined;
          // Spacer between last session row and first "+ new" row.
          const spacer =
            firstNewIndex > 0 && i === firstNewIndex ? (
              <box key="__spacer" height={1} flexShrink={0} />
            ) : null;
          if (row.kind === "new") {
            const h = HARNESSES.find((h) => h.id === row.harnessId)!;
            return (
              <>
                {spacer}
                <box
                  key={`new:${row.harnessId}`}
                  flexDirection="row"
                  backgroundColor={bg}
                  paddingLeft={1}
                  paddingRight={1}
                >
                  <text fg={selected ? theme.accent : theme.fgDim}>
                    {selected ? "▸ " : "  "}
                  </text>
                  <box width={2} flexShrink={0}>
                    <text fg={selected ? theme.accent : theme.fgDim}>{h.letter}</text>
                  </box>
                  {/* Tweak 3: fixed-width glyph cell */}
                  <box width={2} flexShrink={0}>
                    <text fg={selected ? h.color : theme.fgDim}>{h.glyph}</text>
                  </box>
                  <text fg={selected ? theme.fgBright : theme.fgDim}>
                    new {h.label} session
                  </text>
                </box>
              </>
            );
          }
          const e = row.entry;
          const h = HARNESSES.find((h) => h.id === e.harnessId)!;
          const ageText =
            e.lastActiveMs !== null
              ? ageMsToText(Date.now() - e.lastActiveMs)
              : null;
          const state: DerivedState | null = e.extras.derivedState;
          // Claude entries show derived state + state-tinted dot;
          // codex / opencode entries show simple "live" / "dead" in
          // dim color. Picker is the only place this differs.
          const labelFg = selected
            ? theme.fgBright
            : state === "idle"
              ? theme.fgDim
              : theme.fg;
          const stateFg = state ? STATE_FG[state] : theme.fgDim;
          const statusText = state ? state : e.isLive ? "live" : "dead";
          const showDigit = sessionDigitCursor < 9;
          const prefix = showDigit ? `${sessionDigitCursor + 1}` : " ";
          if (showDigit) sessionDigitCursor++;
          return (
            <box
              key={`s:${e.harnessId}:${e.sessionId}`}
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
              {/* Tweak 3: fixed-width glyph cell */}
              <box width={2} flexShrink={0}>
                <text fg={h.color}>{h.glyph}</text>
              </box>
              <box flexGrow={1} flexShrink={1} overflow="hidden">
                <text fg={labelFg} wrapMode="none" truncate>
                  {e.displayName}
                </text>
              </box>
              {e.extras.queued > 0 ? (
                <text fg={theme.warn}>{e.extras.queued}⏵ </text>
              ) : null}
              {/* Tweak 1: fixed-width status and age columns */}
              <box width={11} flexShrink={0} justifyContent="flex-end">
                {state ? (
                  <text fg={stateFg}>
                    {STATE_DOT[state]} {statusText}
                  </text>
                ) : (
                  <text fg={stateFg}>{statusText}</text>
                )}
              </box>
              <box width={6} flexShrink={0} justifyContent="flex-end">
                <text fg={theme.fgDim}>{ageText ?? ""}</text>
              </box>
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
          <SummaryPanel
            row={rows[selectedIndex] ?? null}
            summaries={summaries}
          />
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
   * caller via `nextAutoName`. Claude-only flow.
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
      title={`new claude session · ${slug}`}
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
