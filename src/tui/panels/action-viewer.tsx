import type { ActionLine, ActionRun } from "../../core/actions.ts";
import { useSessionRun } from "../hooks/useSessionRun.ts";
import { theme } from "../theme.ts";

function fmtTime(ts: number): string {
  return new Date(ts).toTimeString().slice(0, 8);
}

function lineFg(kind: ActionLine["kind"]): string {
  switch (kind) {
    case "info":
      return theme.fgDim;
    case "user":
      return theme.accent;
    case "assistant":
      return theme.fg;
    case "tool":
      return theme.accentAlt;
    case "tool-result":
      return theme.fgDim;
    case "stdout":
      return theme.fg;
    case "stderr":
      return theme.warn;
    case "exit-success":
      return theme.ok;
    case "exit-failure":
      return theme.err;
  }
}

function actionStatusColor(run: ActionRun): string {
  if (run.status === "running") return theme.accent;
  if (run.status === "succeeded") return theme.ok;
  if (run.status === "killed") return theme.warn;
  return theme.err;
}

function actionStatusLabel(run: ActionRun): string {
  if (run.status === "running") return "running";
  if (run.status === "succeeded") return "done";
  if (run.status === "killed") return "killed";
  return "failed";
}

type LinesPanelProps = {
  title: string;
  borderColor: string;
  height: number;
  lines: readonly ActionLine[];
};

/**
 * Shared chrome for the action viewer and the session viewer. Renders
 * the tail of the line buffer that fits — no scrollback, no wrapping;
 * long messages already arrive split per source line so a 20-line model
 * response shows the last N that fit and clips the rest. Status info
 * lives in the title; there's no footer.
 */
function LinesPanel({ title, borderColor, height, lines }: LinesPanelProps) {
  const visibleRows = Math.max(1, height - 2);
  const visible = lines.slice(-visibleRows);
  return (
    <box
      flexShrink={0}
      height={height}
      border
      borderStyle="single"
      borderColor={borderColor}
      title={title}
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
    </box>
  );
}

/**
 * Replaces the activity pane when the selected worktree has a running
 * or recently-finished `claude -p` action.
 */
export function ActionViewer({
  run,
  height,
}: {
  run: ActionRun;
  height: number;
}) {
  const color = actionStatusColor(run);
  const killHint = run.status === "running" ? " · ! kill" : "";
  return (
    <LinesPanel
      title={` action · ${run.actionName} · ${actionStatusLabel(run)}${killHint} `}
      borderColor={color}
      height={height}
      lines={run.lines}
    />
  );
}

/**
 * Replaces the activity pane when the selected worktree has a live
 * interactive F12 session. No "running/done" state machine — the
 * tailer is only registered while the tmux session is live, so this
 * only renders during a live session.
 *
 * Subscribes to `useSessionRun(slug)` here (rather than the parent App)
 * so registry commits during a busy claude turn re-render only this
 * pane, not the entire app tree.
 */
export function SessionViewer({
  slug,
  height,
}: {
  slug: string;
  height: number;
}) {
  const run = useSessionRun(slug);
  // No data yet: the tailer is in its pre-creation race (jsonl not on
  // disk) or we just mounted in the same frame the registry was first
  // populated. Surface a placeholder line so the pane doesn't read as
  // "claude session is silent" when it's actually starting up.
  const lines: readonly ActionLine[] =
    run && run.lines.length > 0
      ? run.lines
      : [
          {
            ts: run?.startedAt ?? Date.now(),
            kind: "info",
            text: "  waiting for claude session output…",
          },
        ];
  return (
    <LinesPanel
      title=" session · F12 enter · ⇧F12 kill "
      // Mauve to keep this pane visually distinct from the action
      // viewer's cyan/green/red/yellow status palette — the two never
      // render simultaneously but they swap back-to-back, so a shared
      // border color would read as "same view, just refreshed".
      borderColor={theme.info}
      height={height}
      lines={lines}
    />
  );
}
