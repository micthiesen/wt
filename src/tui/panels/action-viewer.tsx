/**
 * Inner content renderers for the unified OutputViewer. The
 * surrounding `<box>` (border, title, padding) is owned by
 * OutputViewer; these emit just the line rows.
 */
import type { ActionLine, ActionRun } from "../../core/actions.ts";
import type { TailHarnessId } from "../../core/harness/tail.ts";
import { getHarness } from "../../core/harness/index.ts";
import { actionLineFg } from "../action-line-style.ts";
import {
  useHarnessRun,
  useSessionRun,
  useShellRun,
} from "../hooks/useSessionRun.ts";
import { theme } from "../theme.ts";

function fmtTime(ts: number): string {
  return new Date(ts).toTimeString().slice(0, 8);
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
      {visible.map((line) => (
        <box
          key={line.id}
          flexDirection="row"
          flexShrink={0}
          overflow="hidden"
        >
          <box flexShrink={0} flexDirection="row">
            <text fg={theme.fgDim}>{fmtTime(line.ts)}</text>
            <text> </text>
          </box>
          <box flexGrow={1} flexShrink={1} overflow="hidden">
            <text fg={actionLineFg(line.kind)} wrapMode="none" truncate>
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
 * Subscribes via `useSessionRun(slug, name)` so registry commits during
 * a busy claude turn re-render only this content, not the whole tree.
 * `name = null` is the primary; a string is one of the named sessions
 * spawned via Shift+F12.
 */
export function SessionContent({
  slug,
  name,
  height,
}: {
  slug: string;
  name?: string | null;
  height: number;
}) {
  const run = useSessionRun(slug, name ?? null);
  // No data yet: the tailer is in its pre-creation race (jsonl not on
  // disk) or we just mounted in the same frame the registry was first
  // populated. Surface a placeholder line so the pane doesn't read as
  // "claude session is silent" when it's actually starting up.
  const lines: readonly ActionLine[] =
    run && run.lines.length > 0
      ? run.lines
      : [
          {
            id: 0,
            ts: run?.startedAt ?? Date.now(),
            kind: "info",
            text: "  waiting for claude session output…",
          },
        ];
  return <LinesContent height={height} lines={lines} />;
}

/**
 * Live trail of a codex/opencode session, tailed from its rollout jsonl
 * (codex) or SQLite DB (opencode) via `harnessTailRegistry`. Single slot
 * per slug per harness, so no `name`. Same `ActionLine[]` rows as claude.
 */
export function HarnessSessionContent({
  slug,
  harnessId,
  height,
}: {
  slug: string;
  harnessId: TailHarnessId;
  height: number;
}) {
  const run = useHarnessRun(slug, harnessId);
  const lines: readonly ActionLine[] =
    run && run.lines.length > 0
      ? run.lines
      : [
          {
            id: 0,
            ts: run?.startedAt ?? Date.now(),
            kind: "info",
            text: `  waiting for ${getHarness(harnessId).label} session output…`,
          },
        ];
  return <LinesContent height={height} lines={lines} />;
}

/**
 * Live capture of the F10 shell tmux pane via `tmux pipe-pane`. Lines
 * are plain text post-ANSI-strip; they map cleanly onto the existing
 * `kind: "stdout"` row style so we don't need a separate renderer.
 * Pre-creation race + empty-buffer state surfaces a placeholder for
 * the same reason `SessionContent` does.
 */
export function ShellContent({
  slug,
  height,
}: {
  slug: string;
  height: number;
}) {
  const run = useShellRun(slug);
  const lines: readonly ActionLine[] =
    run && run.lines.length > 0
      ? run.lines.map((l) => ({ id: l.id, ts: l.ts, kind: "stdout", text: l.text }))
      : [
          {
            id: 0,
            ts: run?.startedAt ?? Date.now(),
            kind: "info",
            text: "  waiting for shell session output…",
          },
        ];
  return <LinesContent height={height} lines={lines} />;
}
