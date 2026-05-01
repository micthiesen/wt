import type { ClaudeStatus } from "../../core/claude.ts";
import { humanAge } from "../../core/locks.ts";
import { NF } from "../icons.ts";
import { theme } from "../theme.ts";
import type { RowModule } from "./types.ts";

function ageMsToText(ms: number): string {
  return humanAge(Math.max(0, ms) / 1000);
}

function ClaudeLine({ data }: { data: ClaudeStatus | undefined }) {
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
  const convos = count === 1 ? "1 convo" : `${count} convos`;
  return (
    <text fg={theme.fg} wrapMode="none" truncate>
      <span fg={head.fg}>
        {NF.comment}  {head.text}
      </span>
      <span fg={theme.fgDim}> · {convos}</span>
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
  render: ({ row }) => <ClaudeLine data={row.fields.claude.data} />,
};
