/**
 * Query definitions — pure data, no React. Each exported factory
 * returns a `queryOptions(...)` result, which gives strong type
 * inference from queryKey → queryFn return type at the hook site.
 *
 * This file is a thin barrel over `./queries/*` — see that directory
 * for the actual implementations, grouped by source.
 */

export { worktreesQuery, fetchOriginNow, fetchOriginQuery, wtDirtyQuery, wtLockQuery, wtDeployQuery, wtMergedQuery, wtGoneQuery, wtSyncQuery, wtClaudeQuery, wtGitActivityQuery, wtConflictQuery, wtFirstCommitQuery } from "./queries/worktree.ts";

export { archiveQuery, wtStateQuery } from "./queries/wtstate.ts";

export type { GithubData, ReviewRequestPr } from "./queries/github.ts";
export { githubQuery, reviewRequestsQuery, contributorsQuery } from "./queries/github.ts";

export type { TmuxSessionsData, ClaudeSessionEntry } from "./queries/sessions.ts";
export { tmuxSessionsQuery, harnessSessionsQuery, primaryHarnessQuery } from "./queries/sessions.ts";

export { claudeUsageQuery, codexUsageQuery, opencodeCostQuery } from "./queries/usage.ts";

export type { ClaudeRegistryData } from "./queries/claude.ts";
export { claudeRegistryQuery, claudeSummariesQuery } from "./queries/claude.ts";

export type { StackMember } from "./queries/ai.ts";
export { wtDiffContextQuery, aiSummaryQuery, buildStackSignature, stackTitleQuery } from "./queries/ai.ts";
