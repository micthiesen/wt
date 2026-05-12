import { useQuery } from "@tanstack/react-query";

import { wtSessionUuid, type ClaudeStatus } from "../../core/claude.ts";
import {
  deriveSessionState,
  pickAggregateState,
  type DerivedState,
} from "../../core/claude-status.ts";
import type { Worktree } from "../../core/types.ts";
import { claudeRegistryQuery } from "../../state/queries.ts";
import { STATE_FG } from "../claude-state.ts";
import { useClaudeSessionsForSlug } from "../hooks/useActiveSessions.ts";
import { NF } from "../icons.ts";
import { ageMsToText } from "../text.ts";
import { theme } from "../theme.ts";
import type { RowModule } from "./types.ts";

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
    const uuid = wtSessionUuid(wt.path, tail.name);
    const reg = bySessionId?.[uuid] ?? null;
    return {
      tail,
      state: deriveSessionState(tail, liveSet.has(tail.name), reg?.status ?? null),
    };
  });
  // Aggregate over the derived set (non-empty by the early return
  // above). `pickAggregateState` returns null only on empty input.
  const aggState = pickAggregateState(derived.map((d) => d.state)) ?? "idle";
  const counts: Record<DerivedState, number> = {
    working: 0,
    waiting: 0,
    abandoned: 0,
    idle: 0,
  };
  for (const d of derived) counts[d.state]++;
  // Most-recently-active session in the aggregate state — drives the
  // age stat. Single pass; the in-state subset is non-empty by
  // construction.
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
