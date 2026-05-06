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
export type SessionKind = "claude" | "diff" | "shell";
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
  endedAt?: number;
  /** Drives picker sorting. For events: the latest event timestamp. */
  lastActivity: number;
  /** Session sub-kind for `kind: "session"`; otherwise undefined. */
  sessionKind?: SessionKind;
};

const EVENTS_OUTPUT_ID = "events";

export function eventsOutputId(): string {
  return EVENTS_OUTPUT_ID;
}

export function actionOutputId(slug: string, startedAt: number): string {
  return `action:${slug}:${startedAt}`;
}

export function sessionOutputId(slug: string, kind: SessionKind): string {
  return `session:${slug}:${kind}`;
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
    endedAt: run.endedAt,
    lastActivity,
  };
}

export function sessionOutput(
  slug: string,
  kind: SessionKind,
  startedAt: number,
  lastActivity: number,
): Output {
  const label =
    kind === "claude" ? "F12 claude" : kind === "diff" ? "F11 diff" : "F10 shell";
  return {
    id: sessionOutputId(slug, kind),
    kind: "session",
    title: `${slug} · ${label}`,
    slug,
    sessionKind: kind,
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
