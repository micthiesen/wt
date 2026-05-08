import type { ClaudeStatus, SessionTail } from "../../core/claude.ts";
import { humanAge } from "../../core/locks.ts";
import { useClaudeSessionsForSlug } from "../hooks/useActiveSessions.ts";
import { NF } from "../icons.ts";
import { theme } from "../theme.ts";
import type { RowModule } from "./types.ts";

/**
 * Per-session derived state. The state machine is two signals — last
 * jsonl entry (from `SessionTail.lastEntryKind`) and tmux liveness for
 * `(slug, name)`. No heuristic age windows: a session "waiting" for a
 * week is still waiting; a session "working" silently for an hour is
 * still working as long as tmux holds it. Age is reported as a stat,
 * never as a state input.
 *
 *   - working   — tmux live + last entry is mid-turn (tool_use /
 *                 tool_result / paused). Claude is busy.
 *   - waiting   — tmux live + last entry is end_turn (or unrecognized).
 *                 Ready for input.
 *   - abandoned — tmux dead + last entry is mid-turn. Process died
 *                 without finishing — actionable, surfaces explicitly.
 *   - idle      — tmux dead + end_turn (or unclassified entry).
 *                 Resumable ghost; no live process.
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

function deriveState(tail: SessionTail, isLive: boolean): DerivedState {
  const midTurn =
    tail.lastEntryKind === "tool_use" ||
    tail.lastEntryKind === "tool_result" ||
    tail.lastEntryKind === "paused";
  if (isLive) return midTurn ? "working" : "waiting";
  return midTurn ? "abandoned" : "idle";
}

function ageMsToText(ms: number): string {
  return humanAge(Math.max(0, ms) / 1000);
}

function ClaudeLine({
  data,
  slug,
}: {
  data: ClaudeStatus | undefined;
  slug: string;
}) {
  const liveNames = useClaudeSessionsForSlug(slug);

  if (!data || data.sessions.length === 0) {
    return <text fg={theme.fgDim}>—</text>;
  }

  const liveSet = new Set(liveNames);
  const derived = data.sessions.map((tail) => ({
    tail,
    state: deriveState(tail, liveSet.has(tail.name)),
  }));
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
    <ClaudeLine data={row.fields.claude.data} slug={row.wt.slug} />
  ),
};
