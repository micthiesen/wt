import { useEvents, type WtEvent } from "../events.ts";
import { theme } from "../theme.ts";

function levelFg(level: WtEvent["level"]): string {
  switch (level) {
    case "ok":
      return theme.ok;
    case "warn":
      return theme.warn;
    case "err":
      return theme.err;
    case "info":
      return theme.fg;
    case "dim":
    default:
      return theme.fgDim;
  }
}

function fmtTime(ts: number): string {
  return new Date(ts).toTimeString().slice(0, 8);
}

/**
 * Non-slug sources are bracketed by convention (e.g. `[app]`, `[prs]`,
 * `[origin]`). They log cross-cutting system events rather than
 * activity on a specific worktree, so they render dimmer — the bright
 * accent color is reserved for slug-tagged rows.
 */
function sourceFg(source: string): string {
  return source.startsWith("[") ? theme.fgDim : theme.accentAlt;
}

/**
 * Inner content for the events tail — caller owns the surrounding
 * `<box>` chrome. Used by the unified `OutputViewer`; the standalone
 * `ActivityPane` below wraps this with its own border + title.
 */
export function ActivityContent({ height }: { height: number }) {
  const events = useEvents();
  // Take just the tail that fits. Rendering the entire buffer each
  // frame is cheap (~500 lines max) but pointless — only the last N
  // are visible anyway, and flat boxes are ideal for the layout.
  const visible = events.slice(-Math.max(1, height - 2));

  if (visible.length === 0) {
    return <text fg={theme.fgDim}>(no events yet)</text>;
  }
  return (
    <>
      {visible.map((e) => (
        // Each event has to stay exactly one row. The prefix
        // (time + source) is grouped into a flexShrink=0 container
        // so flex pressure from a long message can only shrink the
        // message column — without this wrapping, the bare
        // `<text> </text>` spacers get zero-width-collapsed under
        // pressure, jamming the time+source columns together.
        // `overflow="hidden"` on the row clips any residual overrun.
        <box key={e.id} flexDirection="row" flexShrink={0} overflow="hidden">
          <box flexShrink={0} flexDirection="row">
            <text fg={theme.fgDim}>{fmtTime(e.ts)}</text>
            <text> </text>
            <text fg={sourceFg(e.source)}>
              {e.source.slice(0, 16).padStart(16)}
            </text>
            <text> </text>
          </box>
          <box flexGrow={1} flexShrink={1} overflow="hidden">
            <text fg={levelFg(e.level)} wrapMode="none" truncate>
              {e.text}
            </text>
          </box>
        </box>
      ))}
    </>
  );
}

