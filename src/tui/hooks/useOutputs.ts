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
  destroyOutput,
  eventsOutput,
  sessionOutput,
  sortOutputs,
} from "../../core/outputs.ts";
import { sessionTailRegistry } from "../../core/session-tail.ts";
import { tmuxSessionsQuery } from "../../state/queries.ts";
import { events as eventsLog } from "../events.ts";

/**
 * Per-(slug, kind) first-seen timestamp for diff/shell sessions.
 * Module-level so re-derivations of the `useOutputs` memo don't reset
 * the clock. Pruned against the live tmux session set so a closed +
 * reopened session restarts its activity stamp — otherwise we'd hand
 * out a stale 6-hour-old timestamp from the previous attach.
 */
const firstSeen = {
  diff: new Map<string, number>(),
  shell: new Map<string, number>(),
};

function firstSeenStamp(kind: "diff" | "shell", slug: string): number {
  const map = firstSeen[kind];
  let ts = map.get(slug);
  if (ts === undefined) {
    ts = Date.now();
    map.set(slug, ts);
  }
  return ts;
}

function pruneFirstSeen(kind: "diff" | "shell", live: readonly string[]): void {
  const map = firstSeen[kind];
  if (map.size === 0) return;
  const liveSet = new Set(live);
  for (const slug of map.keys()) {
    if (!liveSet.has(slug)) map.delete(slug);
  }
}

export function useOutputs(opts: {
  /** Slugs whose lock op is `"remove"` — drives the destroy outputs. */
  destroyingSlugs: readonly string[];
}): readonly Output[] {
  const { destroyingSlugs } = opts;
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

    // Destroy in flight: one entry per destroying slug. `lastActivity`
    // is the ts of the latest event tagged with `source = slug` so
    // sort order tracks real progress; falls back to "now" until the
    // first line lands.
    if (destroyingSlugs.length > 0) {
      const lastBySlug = new Map<string, number>();
      for (const e of evts) {
        if (destroyingSlugs.includes(e.source)) lastBySlug.set(e.source, e.ts);
      }
      const fallback = Date.now();
      for (const slug of destroyingSlugs) {
        out.push(destroyOutput(slug, lastBySlug.get(slug) ?? fallback));
      }
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
      // they were created (tmuxSessionsQuery only returns slugs),
      // so we cache the first-seen timestamp per (slug, kind) and
      // reuse it across re-derivations. Without this, every memo
      // recompute (every event line, every claude tail line) would
      // stamp `Date.now()` and these placeholder outputs would
      // constantly tie or beat the real claude session in the sort,
      // causing the live group order to flicker. Entries are
      // pruned for sessions that are no longer live so a slug that
      // re-attaches restarts its first-seen clock.
      pruneFirstSeen("diff", sessions.diff);
      pruneFirstSeen("shell", sessions.shell);
      for (const slug of sessions.diff) {
        const ts = firstSeenStamp("diff", slug);
        out.push(sessionOutput(slug, "diff", ts, ts));
      }
      for (const slug of sessions.shell) {
        const ts = firstSeenStamp("shell", slug);
        out.push(sessionOutput(slug, "shell", ts, ts));
      }
    }

    return sortOutputs(out);
  }, [evts, actions, tails, sessions, destroyingSlugs]);
}
