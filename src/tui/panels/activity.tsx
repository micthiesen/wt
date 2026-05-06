import { useMemo } from "react";

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
 * Pure renderer for an event tail — caller owns events and chrome.
 * Used by `ActivityContent` (full event log) and `DestroyContent`
 * (events filtered to a single slug).
 */
function EventsList({
  events,
  height,
  emptyText,
}: {
  events: readonly WtEvent[];
  height: number;
  emptyText: string;
}) {
  // Take just the tail that fits. Rendering the entire buffer each
  // frame is cheap (~500 lines max) but pointless — only the last N
  // are visible anyway, and flat boxes are ideal for the layout.
  const visible = events.slice(-Math.max(1, height - 2));
  if (visible.length === 0) {
    return <text fg={theme.fgDim}>{emptyText}</text>;
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

/**
 * Inner content for the events tail — caller owns the surrounding
 * `<box>` chrome. Rendered inside `OutputViewer`'s border when the
 * `events` output is selected.
 */
export function ActivityContent({ height }: { height: number }) {
  const events = useEvents();
  return (
    <EventsList events={events} height={height} emptyText="(no events yet)" />
  );
}

/**
 * Inner content for an in-flight destroy — events filtered to that
 * slug's source. Destroy logs are tailed by `useLogTails` and pushed
 * into the global events log under `source = <slug>`, so this view
 * is the right slice rather than a separate buffer. Slug-tagged
 * non-destroy events for the same slug also land here, but during a
 * destroy the destroy lines dominate by volume.
 */
export function DestroyContent({
  slug,
  height,
}: {
  slug: string;
  height: number;
}) {
  const events = useEvents();
  const filtered = useMemo(
    () => events.filter((e) => e.source === slug),
    [events, slug],
  );
  return (
    <EventsList
      events={filtered}
      height={height}
      emptyText="(waiting for destroy output…)"
    />
  );
}
