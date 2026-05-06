/**
 * Inner content renderers for the unified OutputViewer. The
 * surrounding `<box>` (border, title, padding) is owned by
 * OutputViewer; these emit just the line rows.
 */
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

type LinesProps = {
  height: number;
  lines: readonly ActionLine[];
};

/**
 * Renders the trailing window that fits the available height; long
 * messages already arrive split per source line so a 20-line model
 * response shows the last N that fit and clips the rest.
 */
function LinesContent({ height, lines }: LinesProps) {
  const visibleRows = Math.max(1, height - 2);
  const visible = lines.slice(-visibleRows);
  return (
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
  );
}

export function ActionContent({
  run,
  height,
}: {
  run: ActionRun;
  height: number;
}) {
  return <LinesContent height={height} lines={run.lines} />;
}

/**
 * Live capture of the wt-managed interactive `claude` session jsonl.
 * Subscribes via `useSessionRun(slug)` so registry commits during a
 * busy claude turn re-render only this content, not the whole tree.
 */
export function SessionContent({
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
  return <LinesContent height={height} lines={lines} />;
}
