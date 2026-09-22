import { useEffect, useRef } from "react";

import { config } from "../../core/config.ts";
import type { IssueStatuses } from "../../core/issue-status.ts";
import { createLogger } from "../../core/logger.ts";
import { useIssueStatuses } from "../../state/hooks.ts";

export type IssueStatusObservation = {
  source: string;
  statuses: IssueStatuses | undefined;
};

/** Missing fetch data is not a transition. A changed reader seeds afresh. */
export function observeIssueStatuses(
  previous: IssueStatusObservation | null,
  source: string,
  statuses: IssueStatuses | undefined,
): { observation: IssueStatusObservation; lines: string[] } {
  const baseline = previous?.source === source ? previous.statuses : undefined;
  if (statuses === undefined) {
    return { observation: { source, statuses: baseline }, lines: [] };
  }
  const lines: string[] = [];
  if (baseline) {
    for (const [id, status] of Object.entries(statuses)) {
      if (!Object.hasOwn(baseline, id) || baseline[id] === status) continue;
      lines.push(`#${id}: ${baseline[id]} → ${status}`);
    }
  }
  // Replace rather than merge: removed IDs are history if they reappear.
  return { observation: { source, statuses }, lines };
}

/** Narrate only server-confirmed transitions; first sightings stay quiet. */
export function useIssueStatusEvents(): void {
  const { confirmedData, ids } = useIssueStatuses();
  const source = JSON.stringify([
    config.issueTracker?.statusCommand ?? null,
    config.paths.mainClone,
  ]);
  const seenRef = useRef<IssueStatusObservation | null>(null);
  useEffect(() => {
    const { observation, lines } = observeIssueStatuses(seenRef.current, source, ids.length ? confirmedData : {});
    seenRef.current = observation;
    const log = createLogger("issues");
    for (const line of lines) log.attention.info(line, { toast: false });
  }, [source, confirmedData, ids]);
}
