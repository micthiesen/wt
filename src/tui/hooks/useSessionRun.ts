/**
 * Observer hooks for the session-tail registries (claude jsonl + shell
 * pipe-pane). The registries' snapshot identities change on every
 * line append, so consumers re-render on each new entry without
 * needing a polling effect.
 */
import { useSyncExternalStore } from "react";

import {
  type HarnessRun,
  type TailHarnessId,
  harnessTailKey,
  harnessTailRegistry,
} from "../../core/harness/harness-tail.ts";
import {
  type SessionRun,
  sessionTailRegistry,
  tailKey,
} from "../../core/session-tail.ts";
import {
  type ShellRun,
  shellTailRegistry,
} from "../../core/shell-tail.ts";

/**
 * Live session tail for a (slug, name) pair. `name` defaults to null
 * (primary) so existing callers reading the primary's tail don't
 * change.
 */
export function useSessionRun(
  slug: string | undefined,
  name: string | null = null,
): SessionRun | null {
  const map = useSyncExternalStore(
    sessionTailRegistry.subscribe,
    sessionTailRegistry.getSnapshot,
    sessionTailRegistry.getSnapshot,
  );
  if (!slug) return null;
  return map.get(tailKey(slug, name)) ?? null;
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

/**
 * Live tail for a codex/opencode slot (single slot per slug per
 * harness). Backed by `harnessTailRegistry`, which polls the rollout
 * jsonl / SQLite and produces the same `ActionLine[]` shape as claude.
 */
export function useHarnessRun(
  slug: string | undefined,
  harnessId: TailHarnessId,
): HarnessRun | null {
  const map = useSyncExternalStore(
    harnessTailRegistry.subscribe,
    harnessTailRegistry.getSnapshot,
    harnessTailRegistry.getSnapshot,
  );
  if (!slug) return null;
  return map.get(harnessTailKey(slug, harnessId)) ?? null;
}
