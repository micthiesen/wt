/**
 * "Fresh" = a FETCH-driven success on the github query this session.
 * Deliberately not `dataUpdatedAt > appStart`: optimistic patches
 * (`setQueriesData` in the mark-ready / auto-merge / reviewer flows)
 * bump `dataUpdatedAt` on the whole cached blob without any network
 * round-trip, which would forge freshness for every OTHER PR still
 * sitting on restored persisted data. The cache subscription filters
 * to non-manual successes — the same manual-flag discrimination the
 * clobber guard in `runOptimisticMutation` uses.
 *
 * Extracted from `useAutomations` so the hub's task-inbox derivation
 * can honor the same hard rule (CLAUDE.md: persisted-cache PR data
 * must never fire) without duplicating the subscription. `enabled`
 * short-circuits the subscription for callers that only conditionally
 * care (automations gate on `configured`); once fresh, stays fresh
 * for the process lifetime.
 */
import { useEffect, useState } from "react";
import { useQueryClient } from "@tanstack/react-query";

export function useGithubFresh(enabled = true): boolean {
  const qc = useQueryClient();
  const [githubFresh, setGithubFresh] = useState(false);
  useEffect(() => {
    if (!enabled || githubFresh) return;
    const unsubscribe = qc.getQueryCache().subscribe((event) => {
      if (event.type !== "updated") return;
      if (event.action.type !== "success") return;
      if ((event.action as { manual?: boolean }).manual) return;
      if (event.query.queryKey[0] !== "github") return;
      setGithubFresh(true);
    });
    return unsubscribe;
  }, [enabled, githubFresh, qc]);
  return githubFresh;
}
