/**
 * Cross-source enumeration of every output the bottom pane could
 * render: the global event log, in-flight + recently-completed action
 * runs, and live tmux sessions (claude / diff / shell). Returns a
 * sorted, stable list and re-evaluates on any underlying source
 * mutation.
 *
 * Only F12 claude sessions have a content tail registered in
 * `core/session-tail.ts`. F10 shell and F11 diff appear in the
 * picker for awareness; selecting them in the OutputViewer renders
 * a "live · attach to view" placeholder.
 */
import { useMemo, useSyncExternalStore } from "react";
import { useQuery } from "@tanstack/react-query";

import { actionRegistry } from "../../core/actions.ts";
import {
  type Output,
  actionOutput,
  eventsOutput,
  sessionOutput,
  sortOutputs,
} from "../../core/outputs.ts";
import { sessionTailRegistry } from "../../core/session-tail.ts";
import { tmuxSessionsQuery } from "../../state/queries.ts";
import { events as eventsLog } from "../events.ts";

export function useOutputs(): readonly Output[] {
  const evts = useSyncExternalStore(
    eventsLog.subscribe,
    eventsLog.getSnapshot,
    eventsLog.getSnapshot,
  );
  const actions = useSyncExternalStore(
    actionRegistry.subscribe,
    actionRegistry.getSnapshot,
    actionRegistry.getSnapshot,
  );
  // Subscribed too — `lastActivity` for a claude session is the
  // timestamp of the most recent line in its tail. Without
  // subscribing, the picker would order sessions by their
  // first-seen-now() stamp forever, so an idle 2h-old session would
  // sort above a busy 5m-old one.
  const tails = useSyncExternalStore(
    sessionTailRegistry.subscribe,
    sessionTailRegistry.getSnapshot,
    sessionTailRegistry.getSnapshot,
  );
  const sessions = useQuery(tmuxSessionsQuery()).data;

  return useMemo(() => {
    const out: Output[] = [];

    const lastEvtTs = evts[evts.length - 1]?.ts ?? Date.now();
    out.push(eventsOutput(lastEvtTs));

    for (const run of actions.values()) {
      out.push(actionOutput(run));
    }

    if (sessions) {
      // Claude sessions: real per-line activity from the tail.
      for (const slug of sessions.claude) {
        const tail = tails.get(slug);
        const startedAt = tail?.startedAt ?? Date.now();
        const lastLineTs = tail?.lines[tail.lines.length - 1]?.ts;
        const lastActivity = lastLineTs ?? startedAt;
        out.push(sessionOutput(slug, "claude", startedAt, lastActivity));
      }
      // Diff/shell sessions: no content tail. We don't know when
      // they were created (tmuxSessionsQuery only returns slugs), so
      // `startedAt` and `lastActivity` are best-effort `now`. They
      // still appear in the picker for awareness; the OutputViewer
      // renders a placeholder when selected.
      const sessNow = Date.now();
      for (const slug of sessions.diff) {
        out.push(sessionOutput(slug, "diff", sessNow, sessNow));
      }
      for (const slug of sessions.shell) {
        out.push(sessionOutput(slug, "shell", sessNow, sessNow));
      }
    }

    return sortOutputs(out);
  }, [evts, actions, tails, sessions]);
}
