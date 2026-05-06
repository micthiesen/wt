/**
 * Observer hooks for the session-tail registries (claude jsonl + shell
 * pipe-pane). The registries' snapshot identities change on every
 * line append, so consumers re-render on each new entry without
 * needing a polling effect.
 */
import { useSyncExternalStore } from "react";

import {
  type SessionRun,
  sessionTailRegistry,
} from "../../core/session-tail.ts";
import {
  type ShellRun,
  shellTailRegistry,
} from "../../core/shell-tail.ts";

export function useSessionRun(slug: string | undefined): SessionRun | null {
  const map = useSyncExternalStore(
    sessionTailRegistry.subscribe,
    sessionTailRegistry.getSnapshot,
    sessionTailRegistry.getSnapshot,
  );
  if (!slug) return null;
  return map.get(slug) ?? null;
}

export function useShellRun(slug: string | undefined): ShellRun | null {
  const map = useSyncExternalStore(
    shellTailRegistry.subscribe,
    shellTailRegistry.getSnapshot,
    shellTailRegistry.getSnapshot,
  );
  if (!slug) return null;
  return map.get(slug) ?? null;
}
