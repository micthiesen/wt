/**
 * Cross-source index over everything that can render in the bottom pane:
 * the global event log, per-worktree action runs (running and recently
 * completed), and live interactive tmux sessions (F10 shell, F12 claude).
 * F11 diff sessions are intentionally excluded — they're a TUI app whose
 * "output" is the diff itself, which the user can already see via the
 * details pane and `core/diff/`. Surfacing F11 here would only ever
 * render an attach-prompt stub.
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

export type OutputKind = "events" | "action" | "session" | "destroy";
/**
 * Subkind of a `kind: "session"` output. Drives both the title label
 * and the OutputViewer's content dispatch. claude/shell tail via
 * `core/harness/claude/tail.ts` (stream-json / pipe-pane); codex + opencode
 * tail via `core/harness/tail.ts` (rollout jsonl / SQLite).
 * F11 diff is deliberately excluded (see file header).
 */
export type OutputSessionKind = "claude" | "shell" | "codex" | "opencode";
export type OutputStatus = "live" | "running" | "done" | "failed" | "killed";

export type Output = {
  id: string;
  kind: OutputKind;
  /** Single-line label rendered in the picker and pane title. */
  title: string;
  /** Worktree slug this output belongs to. `undefined` for global (events). */
  slug?: string;
  /** Session sub-kind for `kind: "session"`; otherwise undefined. */
  sessionKind?: OutputSessionKind;
  /**
   * For `kind: "session"` and `sessionKind: "claude"`, the user-typed
   * session name; `null` is the primary. `null` for non-claude /
   * non-session outputs. Carried through so the OutputViewer / picker
   * can resolve back to the right tmux session and tail registry key.
   */
  sessionName: string | null;
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

export function sessionOutputId(
  slug: string,
  kind: OutputSessionKind,
  name: string | null = null,
): string {
  // Primary claude (and shell, which has no naming concept) use the
  // two-segment id so a slug's pin/focus state stays consistent
  // across the primary's lifecycle. Named claudes get a fourth
  // segment; the per-name id makes parallel sessions individually
  // pinnable.
  if (kind === "claude" && name !== null) {
    return `session:${slug}:${kind}:${name}`;
  }
  return `session:${slug}:${kind}`;
}

export function destroyOutputId(slug: string): string {
  return `destroy:${slug}`;
}

const SESSION_LABEL: Record<OutputSessionKind, string> = {
  claude: "F12 claude",
  shell: "F10 shell",
  codex: "codex",
  opencode: "opencode",
};

export function eventsOutput(lastEventTs: number): Output {
  return {
    id: EVENTS_OUTPUT_ID,
    kind: "events",
    title: "events",
    sessionName: null,
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
    sessionName: null,
    status,
    startedAt: run.startedAt,
    lastActivity,
  };
}

/**
 * In-flight worktree destroy for a slug. Surfaces the per-destroy log
 * file's content (already routed through the global events log via
 * `useLogTails` under `source = slug`) so the user can navigate to
 * watch progress in a dedicated pane instead of reading destroy
 * output mixed with everything else in the events tail. Once the
 * destroy completes the worktree disappears from the rows list and
 * this output disappears with it; same path on destroy *failure* —
 * the lock releases and the entry vanishes. Either way `wt logs
 * <slug>` from the CLI is the on-disk record.
 */
export function destroyOutput(
  slug: string,
  startedAt: number,
  lastActivity: number,
): Output {
  return {
    id: destroyOutputId(slug),
    kind: "destroy",
    title: `${slug} · remove`,
    slug,
    sessionName: null,
    status: "running",
    startedAt,
    lastActivity,
  };
}

/**
 * Live tmux session for a slug. Both F10 shell and F12 claude have a
 * content tail registered in `core/harness/claude/tail.ts`, so the
 * OutputViewer renders running content for both. F11 diff is not an
 * output kind — see the file header.
 *
 * `name` is the user-typed claude session name; `null` is the primary.
 * Ignored for shell (which has no naming concept).
 */
export function sessionOutput(
  slug: string,
  kind: OutputSessionKind,
  startedAt: number,
  lastActivity: number,
  name: string | null = null,
): Output {
  const isNamed = kind === "claude" && name !== null;
  const title = isNamed
    ? `${slug} · ${SESSION_LABEL[kind]} · ${name}`
    : `${slug} · ${SESSION_LABEL[kind]}`;
  return {
    id: sessionOutputId(slug, kind, name),
    kind: "session",
    title,
    slug,
    sessionKind: kind,
    sessionName: kind === "claude" ? name : null,
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

/**
 * Outputs visible to the bottom pane when sitting on `slug`. Slug-
 * tagged outputs (actions, sessions) are filtered to that slug;
 * global outputs (events) come through regardless. When `slug` is
 * null — no row selected — only globals are shown.
 *
 * The Outputs picker, cycle keys, and the per-slug pin/focus state
 * all consult the same filter so the user sees one consistent
 * "outputs for this worktree" universe at every entry point.
 */
export function outputsForSlug(
  items: readonly Output[],
  slug: string | null,
): readonly Output[] {
  if (!slug) return items.filter((o) => !o.slug);
  return items.filter((o) => !o.slug || o.slug === slug);
}
