import { queryOptions } from "@tanstack/react-query";
import { config } from "../../core/config.ts";
import { fetchIssueStatuses } from "../../core/issue-status.ts";
import { qk } from "../keys.ts";
import { runQuery } from "./boundary.ts";

export const ISSUE_STATUS_POLL_MS = 3 * 60 * 1000;

/** One reader for the complete inventory, never one subprocess per row. */
export const issueStatusesQuery = (
  ids: readonly string[],
  command = config.issueTracker?.statusCommand ?? null,
  cwd = config.paths.mainClone,
) => queryOptions({
  queryKey: qk.issueStatuses(ids, command, cwd),
  queryFn: ({ signal }) => runQuery(fetchIssueStatuses(command ?? [], ids, cwd), signal),
  enabled: command !== null && ids.length > 0,
  staleTime: 60_000,
  refetchInterval: command !== null && ids.length > 0 ? ISSUE_STATUS_POLL_MS : false,
  // A changed identity/reader is a new fact. Never show an old provider's
  // status as placeholder data for a newly selected provider.
});
