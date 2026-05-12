import { useQuery } from "@tanstack/react-query";

import { wtSessionUuid, type ClaudeStatus, type SessionTail } from "../../core/claude.ts";
import type { RegistryStatus } from "../../core/claude-registry.ts";
import { humanAge } from "../../core/locks.ts";
import type { Worktree } from "../../core/types.ts";
import { claudeRegistryQuery } from "../../state/queries.ts";
import { useClaudeSessionsForSlug } from "../hooks/useActiveSessions.ts";
import { NF } from "../icons.ts";
import { theme } from "../theme.ts";
import type { RowModule } from "./types.ts";

/**
 * Per-session derived state. Two signals drive it:
 *
 *   1. The registry entry from `~/.claude/sessions/<pid>.json`
 *      (authoritative when present — claude itself wrote it, fs.watch
 *      makes us see the flip within FSEvents latency).
 *   2. The jsonl tail (`SessionTail.lastEntryKind`) — fallback when no
 *      claude process is running for this session, used to decide
 *      whether a ghost was mid-turn (abandoned) or finished cleanly
 *      (idle).
 *
 *   - working   — claude is alive and busy (or, with no registry entry,
 *                 tmux is live and the jsonl ends mid-turn).
 *   - waiting   — claude is alive and idle (or tmux is live with an
 *                 end-of-turn jsonl).
 *   - abandoned — no claude process and the jsonl ends mid-turn. The
 *                 process died without finishing — actionable.
 *   - idle      — no claude process, jsonl ended cleanly. Resumable
 *                 ghost.
 *
 * No heuristic age windows. A session "waiting" for a week is still
 * waiting; a session "working" silently for an hour is still working
 * as long as claude itself reports busy. Age is reported as a stat,
 * never as a state input.
 */
type DerivedState = "working" | "waiting" | "abandoned" | "idle";

// Priority for the headline state in a multi-session worktree:
// working > abandoned > waiting > idle. The user's first question is
// "is anything actively working?" — `working` wins if anything is
// busy. `abandoned` ranks above `waiting` so a crashed peer surfaces
// when nothing is busy (the red color drags the eye); when something
// IS busy, the working signal dominates and the user can find the
// abandoned peer through the picker.
const STATE_PRIORITY: readonly DerivedState[] = [
  "working",
  "abandoned",
  "waiting",
  "idle",
];

const STATE_FG: Record<DerivedState, string> = {
  working: theme.accent,
  waiting: theme.warn,
  abandoned: theme.err,
  // `idle` shares fgDim with the empty-state `—` deliberately — a
  // ghost session is visually closer to "nothing happening" than
  // to a warning.
  idle: theme.fgDim,
};

function deriveState(
  tail: SessionTail,
  isTmuxLive: boolean,
  registryStatus: RegistryStatus | null,
): DerivedState {
  if (registryStatus !== null) {
    // Claude is alive (the registry-load step filters dead pids) and
    // told us what it's doing. Trust it over the tmux/jsonl proxy
    // signals — those exist to compensate for not having this.
    return registryStatus === "busy" ? "working" : "waiting";
  }
  const midTurn =
    tail.lastEntryKind === "tool_use" ||
    tail.lastEntryKind === "tool_result" ||
    tail.lastEntryKind === "paused";
  if (isTmuxLive) return midTurn ? "working" : "waiting";
  return midTurn ? "abandoned" : "idle";
}

function ageMsToText(ms: number): string {
  return humanAge(Math.max(0, ms) / 1000);
}

function ClaudeLine({
  data,
  wt,
}: {
  data: ClaudeStatus | undefined;
  wt: Worktree;
}) {
  const liveNames = useClaudeSessionsForSlug(wt.slug);
  const registry = useQuery(claudeRegistryQuery());

  if (!data || data.sessions.length === 0) {
    return <text fg={theme.fgDim}>—</text>;
  }

  const liveSet = new Set(liveNames);
  const bySessionId = registry.data?.bySessionId;
  const derived = data.sessions.map((tail) => {
    // wt-managed sessions key the registry by their deterministic
    // UUID — same function the spawn path uses, so the lookup
    // matches every session wt itself launched. Non-wt-managed
    // claude processes in the same cwd land outside this map and
    // fall back to the jsonl signal.
    const uuid = wtSessionUuid(wt.path, tail.name);
    const reg = bySessionId?.[uuid] ?? null;
    return {
      tail,
      state: deriveState(tail, liveSet.has(tail.name), reg?.status ?? null),
    };
  });
  const counts: Record<DerivedState, number> = {
    working: 0,
    waiting: 0,
    abandoned: 0,
    idle: 0,
  };
  for (const d of derived) counts[d.state]++;
  // `STATE_PRIORITY.find(s => counts[s] > 0)` is guaranteed to hit
  // when `derived.length > 0` (already gated by the early return),
  // so `aggState` always names a state with at least one member.
  // The `?? "idle"` is a defensive default that shouldn't be reachable.
  const aggState = STATE_PRIORITY.find((s) => counts[s] > 0) ?? "idle";
  // Most-recently-active session in the aggregate state — drives the
  // age stat. Single pass; the in-state subset is non-empty by
  // construction (see comment on aggState above).
  const top = derived.reduce<(typeof derived)[number] | null>((best, d) => {
    if (d.state !== aggState) return best;
    if (!best) return d;
    return (d.tail.lastEntryMs ?? 0) > (best.tail.lastEntryMs ?? 0) ? d : best;
  }, null) ?? derived[0]!;
  const totalQueued = data.sessions.reduce((sum, t) => sum + t.queued, 0);
  const total = data.sessions.length;
  const inState = counts[aggState];

  // Head label — "{state}" for N=1, "{n} {state}" when every session
  // is in the same state, "{state} {n}/{N}" when mixed. Compact form
  // keeps the line readable in the narrow details pane.
  let head: string;
  if (total === 1) {
    head = aggState;
  } else if (inState === total) {
    head = `${total} ${aggState}`;
  } else {
    head = `${aggState} ${inState}/${total}`;
  }
  const headFg = STATE_FG[aggState];

  const ageText =
    top.tail.lastEntryMs !== null
      ? ageMsToText(Date.now() - top.tail.lastEntryMs)
      : null;

  return (
    <text fg={theme.fg} wrapMode="none" truncate>
      <span fg={headFg}>
        {NF.comment}  {head}
      </span>
      {ageText ? <span fg={theme.fgDim}> · {ageText}</span> : null}
      {totalQueued > 0 ? (
        <span fg={theme.warn}>
          {" · "}
          {totalQueued} queued
        </span>
      ) : null}
    </text>
  );
}

export const claudeRow: RowModule = {
  id: "claude",
  label: "claude",
  sources: ({ row }) => [row.fields.claude],
  render: ({ row }) => (
    <ClaudeLine data={row.fields.claude.data} wt={row.wt} />
  ),
};
