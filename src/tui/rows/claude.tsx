import type { ClaudeStatus } from "../../core/claude.ts";
import { humanAge } from "../../core/locks.ts";
import { useActiveSessions } from "../hooks/useActiveSessions.ts";
import { NF } from "../icons.ts";
import { theme } from "../theme.ts";
import type { RowModule } from "./types.ts";

function ageMsToText(ms: number): string {
  return humanAge(Math.max(0, ms) / 1000);
}

/**
 * One-line snapshot of what claude is doing right now, drawn from the
 * latest assistant turn in the newest session jsonl. Returns null when
 * there's nothing to show — caller falls back to the convo/queued
 * counts. The text is already compacted/truncated upstream in
 * `core/claude.ts`; we just choose a prefix.
 */
function latestText(data: ClaudeStatus): string | null {
  const l = data.latest;
  if (!l) return null;
  if (l.kind === "assistant") return `“${l.text}”`;
  return l.arg ? `⚒ ${l.name}(${l.arg})` : `⚒ ${l.name}`;
}

function ClaudeLine({
  data,
  slug,
}: {
  data: ClaudeStatus | undefined;
  slug: string;
}) {
  const sessions = useActiveSessions();
  const sessionActive = sessions.has(slug);

  // Nothing to show. Note we don't add a session-alive marker here even
  // when sessionActive is true with no jsonl — the list pane already
  // carries that signal via the cyan comment glyph; duplicating it
  // here just adds visual noise.
  if (!data || data.state.kind === "none") {
    return <text fg={theme.fgDim}>—</text>;
  }

  const now = Date.now();
  const { state, count, queued } = data;
  let head: { text: string; fg: string };
  if (state.kind === "working") {
    head = { text: `working · ${ageMsToText(now - state.lastEntryMs)}`, fg: theme.accent };
  } else if (state.kind === "waiting") {
    head = { text: `waiting · ${ageMsToText(now - state.lastEntryMs)}`, fg: theme.warn };
  } else {
    head = { text: `last ${ageMsToText(now - state.lastEntryMs)}`, fg: theme.fgDim };
  }
  const snippet = latestText(data);
  // When a session is live OR claude is actively working, the snippet is
  // the most useful tail. The convo/queued counts only earn the slot
  // when there's no live activity worth surfacing.
  const showSnippet = !!snippet && (sessionActive || state.kind === "working" || state.kind === "waiting");
  const convos = count === 1 ? "1 convo" : `${count} convos`;
  return (
    <text fg={theme.fg} wrapMode="none" truncate>
      <span fg={head.fg}>
        {NF.comment}  {head.text}
      </span>
      {showSnippet ? (
        <span fg={theme.fgDim}> · {snippet}</span>
      ) : (
        <span fg={theme.fgDim}> · {convos}</span>
      )}
      {queued > 0 ? (
        <span fg={theme.warn}>
          {" · "}
          {queued} queued
        </span>
      ) : null}
    </text>
  );
}

export const claudeRow: RowModule = {
  id: "claude",
  label: "claude",
  sources: ({ row }) => [row.fields.claude],
  render: ({ row }) => (
    <ClaudeLine data={row.fields.claude.data} slug={row.wt.slug} />
  ),
};
