/**
 * Per-session derived state shared by every surface that wants to know
 * "what is this claude session doing right now": the details-pane
 * `claude` row, the sessions picker, the list-pane session-count glyph
 * tint. Single source of truth so a session reads the same state
 * everywhere it's shown.
 *
 * Two signals drive it:
 *   1. The live registry entry from `~/.claude/sessions/<pid>.json`
 *      (authoritative when present — claude itself wrote it).
 *   2. The jsonl tail (`SessionTail.lastEntryKind`) — fallback used
 *      only when no claude process is running for the session, to
 *      decide whether a ghost was abandoned mid-turn or finished
 *      cleanly.
 *
 * The `isTmuxLive` flag survives as a fallback signal for the rare
 * case where claude is running outside the registry (very old build,
 * registry write failed). It loses to the registry when both are
 * present.
 */
import type { SessionTail } from "./claude.ts";
import type { RegistryStatus } from "./claude-registry.ts";

export type DerivedState =
  | "working"
  | "asking"
  | "polling"
  | "unknown"
  | "waiting"
  | "abandoned"
  | "idle";

/**
 * Headline priority for multi-session aggregation. `asking` wins above
 * everything: a session explicitly blocked on the human is the single
 * most actionable thing to surface. Then "is anything actively
 * working?" — `working`, then `polling` (turn done but a background
 * task is still running), then `unknown` (a live session in a status we
 * don't recognize), all still active. `abandoned` ranks above `waiting`
 * so a crashed peer surfaces when nothing is busy.
 */
export const STATE_PRIORITY: readonly DerivedState[] = [
  "asking",
  "working",
  "polling",
  "unknown",
  "abandoned",
  "waiting",
  "idle",
];

/**
 * Map a live registry status to a derived state. The registry only ever
 * describes a live process, so it's authoritative whenever present —
 * `deriveSessionState` consults the jsonl tail only when there's no
 * registry entry. Exported so the list pane can tint its glyph straight
 * from the registry snapshot without synthesizing a tail.
 */
export function registryStatusToState(status: RegistryStatus): DerivedState {
  switch (status) {
    case "busy":
      return "working";
    // `shell` = turn done but a background shell/task is still running
    // (CC polls it). Distinct from idle so the user sees work in flight.
    case "shell":
      return "polling";
    // `waiting` = claude blocked mid-turn on a human (permission /
    // question). `idle` = turn complete, awaiting your next prompt.
    case "waiting":
      return "asking";
    // A live session in a status we don't recognize (newer CC). Surface
    // it honestly rather than guessing working/idle.
    case "unknown":
      return "unknown";
    case "idle":
      return "waiting";
  }
}

export function deriveSessionState(
  tail: SessionTail,
  isTmuxLive: boolean,
  registryStatus: RegistryStatus | null,
): DerivedState {
  if (registryStatus !== null) {
    return registryStatusToState(registryStatus);
  }
  const midTurn =
    tail.lastEntryKind === "tool_use" ||
    tail.lastEntryKind === "tool_result" ||
    tail.lastEntryKind === "paused";
  if (isTmuxLive) return midTurn ? "working" : "waiting";
  return midTurn ? "abandoned" : "idle";
}

/**
 * Pick the highest-priority state across a session set. Returns `null`
 * when the input is empty so the caller can suppress the entire badge
 * rather than render a placeholder.
 */
export function pickAggregateState(
  states: readonly DerivedState[],
): DerivedState | null {
  if (states.length === 0) return null;
  const seen = new Set(states);
  for (const s of STATE_PRIORITY) {
    if (seen.has(s)) return s;
  }
  return null;
}
