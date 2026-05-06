/**
 * Cross-source index over everything that can render in the bottom pane:
 * the global event log, per-worktree action runs (running and recently
 * completed), and live interactive tmux sessions (F10/F11/F12).
 *
 * Outputs is intentionally a thin index — it does not own buffers. The
 * underlying registries (events log, action registry, session-tail
 * registry) keep doing what they do; this module just enumerates them
 * with a shared shape so the picker, viewer, and cycle keys see one
 * uniform list.
 *
 * Identity convention: the `id` is stable across re-derivations so
 * "currently focused" survives churn. Actions key on `startedAt` so a
 * relaunch produces a new id rather than mutating the prior entry.
 */
import type { ActionRun } from "./actions.ts";

export type OutputKind = "events" | "action" | "session";
export type OutputStatus = "live" | "running" | "done" | "failed" | "killed";

export type Output = {
  id: string;
  kind: OutputKind;
  /** Single-line label rendered in the picker and pane title. */
  title: string;
  /** Worktree slug this output belongs to. `undefined` for global (events). */
  slug?: string;
  status: OutputStatus;
  startedAt: number;
  /** Drives picker sorting. For events: the latest event timestamp. */
  lastActivity: number;
};

/** Human-readable label for an Output's status — picker badge + pane title. */
export function outputStatusLabel(status: OutputStatus): string {
  switch (status) {
    case "running":
      return "running";
    case "live":
      return "live";
    case "done":
      return "done";
    case "failed":
      return "failed";
    case "killed":
      return "killed";
  }
}

const EVENTS_OUTPUT_ID = "events";

export function eventsOutputId(): string {
  return EVENTS_OUTPUT_ID;
}

export function actionOutputId(slug: string, startedAt: number): string {
  return `action:${slug}:${startedAt}`;
}

export function sessionOutputId(slug: string): string {
  return `session:${slug}:claude`;
}

export function eventsOutput(lastEventTs: number): Output {
  return {
    id: EVENTS_OUTPUT_ID,
    kind: "events",
    title: "events",
    status: "live",
    startedAt: 0,
    lastActivity: lastEventTs,
  };
}

export function actionOutput(run: ActionRun): Output {
  const status: OutputStatus =
    run.status === "running"
      ? "running"
      : run.status === "succeeded"
        ? "done"
        : run.status === "failed"
          ? "failed"
          : "killed";
  const lastLine = run.lines[run.lines.length - 1];
  const lastActivity =
    run.endedAt ?? lastLine?.ts ?? run.startedAt;
  return {
    id: actionOutputId(run.slug, run.startedAt),
    kind: "action",
    title: `${run.slug} · ${run.actionName}`,
    slug: run.slug,
    status,
    startedAt: run.startedAt,
    lastActivity,
  };
}

/**
 * Live claude session for a slug. We only enumerate claude-kind tmux
 * sessions (F12) because that's the only kind with a content tail
 * registered in `core/session-tail.ts`. Diff (F11) and shell (F10)
 * sessions exist but produce no replayable byte stream — surfacing
 * them here would route the picker into a permanent "waiting for
 * output…" placeholder.
 */
export function sessionOutput(
  slug: string,
  startedAt: number,
  lastActivity: number,
): Output {
  return {
    id: sessionOutputId(slug),
    kind: "session",
    title: `${slug} · F12 claude`,
    slug,
    status: "live",
    startedAt,
    lastActivity,
  };
}

/**
 * Sort outputs for the picker: live ones first (running/live), then
 * completed by recency. Within a group, newer activity beats older.
 * Events always renders last in the live group so it doesn't churn
 * to the top on every dim log line.
 */
export function sortOutputs(items: readonly Output[]): readonly Output[] {
  const liveRank = (s: OutputStatus): number =>
    s === "running" || s === "live" ? 0 : 1;
  const eventsLast = (o: Output): number => (o.kind === "events" ? 1 : 0);
  return [...items].sort((a, b) => {
    const lr = liveRank(a.status) - liveRank(b.status);
    if (lr !== 0) return lr;
    const el = eventsLast(a) - eventsLast(b);
    if (el !== 0) return el;
    return b.lastActivity - a.lastActivity;
  });
}

/**
 * Find the index of `id` in `items`; -1 if missing. Used by the picker
 * and the cycle keybindings.
 */
export function indexOfOutput(items: readonly Output[], id: string): number {
  for (let i = 0; i < items.length; i++) {
    if (items[i]!.id === id) return i;
  }
  return -1;
}
