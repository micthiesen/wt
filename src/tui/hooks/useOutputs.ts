/**
 * Cross-source enumeration of every output the bottom pane could
 * render: the global event log, in-flight + recently-completed action
 * runs, and live tmux sessions. Returns a sorted, stable list and
 * re-evaluates on any underlying source mutation.
 *
 * "Recently completed" tracks the action registry's own retention —
 * `core/actions.ts` keeps finished runs around long enough for the
 * picker; this hook just enumerates whatever's there.
 */
import { useMemo } from "react";
import { useQuery } from "@tanstack/react-query";
import { useSyncExternalStore } from "react";

import { actionRegistry } from "../../core/actions.ts";
import {
  type Output,
  actionOutput,
  eventsOutput,
  sessionOutput,
  sortOutputs,
} from "../../core/outputs.ts";
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
  const sessions = useQuery(tmuxSessionsQuery()).data;

  return useMemo(() => {
    const out: Output[] = [];

    const lastEvtTs = evts[evts.length - 1]?.ts ?? Date.now();
    out.push(eventsOutput(lastEvtTs));

    for (const run of actions.values()) {
      out.push(actionOutput(run));
    }

    if (sessions) {
      const now = Date.now();
      for (const slug of sessions.claude) {
        out.push(sessionOutput(slug, "claude", now, now));
      }
      for (const slug of sessions.diff) {
        out.push(sessionOutput(slug, "diff", now, now));
      }
      for (const slug of sessions.shell) {
        out.push(sessionOutput(slug, "shell", now, now));
      }
    }

    return sortOutputs(out);
  }, [evts, actions, sessions]);
}
