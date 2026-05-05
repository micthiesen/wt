/**
 * Observer hooks for the session-tail registry. `useSessionRun` powers
 * the per-row activity-pane swap (one selected slug); the registry's
 * snapshot identity changes on every line append, so consumers re-render
 * on each new entry without needing a polling effect.
 */
import { useSyncExternalStore } from "react";

import {
  type SessionRun,
  sessionTailRegistry,
} from "../../core/session-tail.ts";

export function useSessionRun(slug: string | undefined): SessionRun | null {
  const map = useSyncExternalStore(
    sessionTailRegistry.subscribe,
    sessionTailRegistry.getSnapshot,
    sessionTailRegistry.getSnapshot,
  );
  if (!slug) return null;
  return map.get(slug) ?? null;
}
