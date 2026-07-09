import { statSync } from "node:fs";

import type { PullRequest, Worktree } from "../types.ts";

/**
 * Resolve the PR that belongs to this worktree's *current era*. A
 * worktree can be rm'd and then recreated on the same branch (e.g. an
 * issue is reopened); on the re-created era the old merged PR is
 * historical, not current. We detect that by comparing the PR's
 * terminal timestamp (mergedAt/closedAt) to the worktree directory's
 * birthtime: a terminal PR that finished before the directory was
 * created belongs to a previous era and is dropped.
 *
 * OPEN PRs are always kept — they have no terminal timestamp, and an
 * open PR on the branch is definitionally current regardless of when
 * it was filed.
 */
export function pickPrForWorktree(
  wt: Worktree,
  prs: Map<string, PullRequest> | Record<string, PullRequest> | undefined,
): PullRequest | undefined {
  if (!wt.branch || !prs) return undefined;
  const pr = prs instanceof Map ? prs.get(wt.branch) : prs[wt.branch];
  if (!pr) return undefined;
  if (pr.state === "OPEN") return pr;
  const terminalAt = pr.mergedAt ?? pr.closedAt;
  if (!terminalAt) return pr;
  let birthMs: number;
  try {
    const st = statSync(wt.path);
    birthMs = st.birthtimeMs || st.ctimeMs;
  } catch {
    // Path vanished (StatusKind.Missing) — no way to gate by birthtime.
    // Keep the PR so the user sees *something* attached to the row.
    return pr;
  }
  return Date.parse(terminalAt) < birthMs ? undefined : pr;
}
