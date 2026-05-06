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
 * Per-(slug, kind) first-seen timestamp for outputs whose underlying
 * source doesn't expose a real start ts: diff/shell sessions
 * (tmuxSessionsQuery only returns slugs) and destroy entries (the
 * fallback when no slug-tagged event has landed yet). Module-level
 * so re-derivations of the `useOutputs` memo don't reset the clock.
 * Pruned against the live set so a closed + reopened session — or a
 * second destroy after a recreate — restarts the activity stamp;
 * otherwise we'd hand out a stale timestamp from the previous run.
 */
type FirstSeenKind = "diff" | "shell" | "destroy";
const firstSeen: Record<FirstSeenKind, Map<string, number>> = {
  diff: new Map(),
  shell: new Map(),
  destroy: new Map(),
};

function firstSeenStamp(kind: FirstSeenKind, slug: string): number {
  const map = firstSeen[kind];
  let ts = map.get(slug);
  if (ts === undefined) {
    ts = Date.now();
    map.set(slug, ts);
  }
  return ts;
}

function pruneFirstSeen(kind: FirstSeenKind, live: readonly string[]): void {
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
    // sort order tracks real progress; the per-slug first-seen
    // timestamp is the stable fallback when no line has landed yet
    // (mirrors the diff/shell session caching above so picker order
    // doesn't flicker on every events recompute).
    if (destroyingSlugs.length > 0) {
      // Set lookup + early break: the events buffer is up to 500
      // lines, scanned on every memo recompute, so per-line work
      // matters. We walk newest → oldest and stop once every
      // destroying slug has a stamp.
      const destroying = new Set(destroyingSlugs);
      const lastBySlug = new Map<string, number>();
      for (let i = evts.length - 1; i >= 0; i--) {
        const e = evts[i]!;
        if (!destroying.has(e.source) || lastBySlug.has(e.source)) continue;
        lastBySlug.set(e.source, e.ts);
        if (lastBySlug.size === destroying.size) break;
      }
      pruneFirstSeen("destroy", destroyingSlugs);
      for (const slug of destroyingSlugs) {
        const startedAt = firstSeenStamp("destroy", slug);
        const lastActivity = lastBySlug.get(slug) ?? startedAt;
        out.push(destroyOutput(slug, startedAt, lastActivity));
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
