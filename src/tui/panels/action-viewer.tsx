import type { ActionLine, ActionRun } from "../../core/actions.ts";
import { theme } from "../theme.ts";

type Props = {
  run: ActionRun;
  height: number;
};

function fmtTime(ts: number): string {
  return new Date(ts).toTimeString().slice(0, 8);
}

function lineFg(kind: ActionLine["kind"]): string {
  switch (kind) {
    case "info":
      return theme.fgDim;
    case "assistant":
      return theme.fg;
    case "tool":
      return theme.accentAlt;
    case "tool-result":
      return theme.fgDim;
    case "exit-success":
      return theme.ok;
    case "exit-failure":
      return theme.err;
  }
}

function statusColor(run: ActionRun): string {
  if (run.status === "running") return theme.accent;
  if (run.status === "succeeded") return theme.ok;
  if (run.status === "killed") return theme.warn;
  return theme.err;
}

function statusLabel(run: ActionRun): string {
  if (run.status === "running") return "running";
  if (run.status === "succeeded") return "done";
  if (run.status === "killed") return "killed";
  return "failed";
}

/**
 * Replaces the activity pane when the selected worktree has a running
 * or recently-finished `claude -p` action. Renders the tail of the
 * line buffer that fits — no scrollback, no wrapping; long messages
 * already arrive split per source line so a 20-line model response
 * shows the last N that fit and clips the rest.
 */
export function ActionViewer({ run, height }: Props) {
  // -2 border, -1 status footer line. Reserved so the footer never
  // gets pushed off by tail growth.
  const visibleRows = Math.max(1, height - 3);
  const visible = run.lines.slice(-visibleRows);

  return (
    <box
      flexShrink={0}
      height={height}
      border
      borderStyle="single"
      borderColor={statusColor(run)}
      title={` action · ${run.actionName} · ${run.slug} `}
      titleAlignment="left"
      flexDirection="column"
      paddingLeft={1}
      paddingRight={1}
    >
      <box flexDirection="column" flexGrow={1} overflow="hidden">
        {visible.map((line, i) => (
          <box
            key={`${line.ts}-${i}`}
            flexDirection="row"
            flexShrink={0}
            overflow="hidden"
          >
            <box flexShrink={0} flexDirection="row">
              <text fg={theme.fgDim}>{fmtTime(line.ts)}</text>
              <text> </text>
            </box>
            <box flexGrow={1} flexShrink={1} overflow="hidden">
              <text fg={lineFg(line.kind)} wrapMode="none" truncate>
                {line.text}
              </text>
            </box>
          </box>
        ))}
      </box>
      <box flexShrink={0} flexDirection="row">
        <text fg={statusColor(run)}>{statusLabel(run)}</text>
        <text fg={theme.fgDim}>
          {" · "}
          {run.lines.length} line{run.lines.length === 1 ? "" : "s"}
          {run.status === "running" ? " · ! to kill" : ""}
        </text>
      </box>
    </box>
  );
}
