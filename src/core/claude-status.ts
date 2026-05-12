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

export type DerivedState = "working" | "waiting" | "abandoned" | "idle";

/**
 * Headline priority for multi-session aggregation. The user's first
 * question is "is anything actively working?" — `working` wins if any
 * session is busy. `abandoned` ranks above `waiting` so a crashed peer
 * surfaces when nothing is busy.
 */
export const STATE_PRIORITY: readonly DerivedState[] = [
  "working",
  "abandoned",
  "waiting",
  "idle",
];

export function deriveSessionState(
  tail: SessionTail,
  isTmuxLive: boolean,
  registryStatus: RegistryStatus | null,
): DerivedState {
  if (registryStatus !== null) {
    return registryStatus === "busy" ? "working" : "waiting";
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
