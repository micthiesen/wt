import { queryOptions } from "@tanstack/react-query";

import { config } from "../../core/config.ts";
import { claudeStatus, type ClaudeStatus } from "../../core/claude.ts";
import { branchIsGone, branchIsMerged, firstCommitSubject, invalidateMainFirstParents, mergeConflictProbe, type MergeConflictProbe } from "../../core/git.ts";
import { gitActivity, type GitActivity } from "../../core/git-activity.ts";
import { lockStatus } from "../../core/locks.ts";
import type {
  LockMeta,
  Worktree,
} from "../../core/types.ts";
import { isOurStageDeployed } from "../../core/stage-safety.ts";
import { fetchOrigin, listWorktrees, syncState, type SyncState, worktreeDirtyFiles } from "../../core/worktree.ts";

import { qk } from "../keys.ts";
import { KEEP_PREV, STALE } from "./shared.ts";

// ---------- Root queries ----------

export const worktreesQuery = () =>
  queryOptions({
    queryKey: qk.worktrees(),
    queryFn: async (): Promise<Worktree[]> => listWorktrees(),
    staleTime: STALE.mid,
  });

export async function fetchOriginNow(): Promise<number> {
  await fetchOrigin();
  invalidateMainFirstParents();
  return Date.now();
}

export const fetchOriginQuery = () =>
  queryOptions({
    queryKey: qk.fetchOrigin(),
    queryFn: fetchOriginNow,
    staleTime: STALE.slow,
  });

// ---------- Per-worktree queries ----------

export const wtDirtyQuery = (wt: Pick<Worktree, "slug" | "path">) =>
  queryOptions({
    queryKey: qk.wt(wt.slug).dirty(),
    queryFn: async (): Promise<readonly string[]> => worktreeDirtyFiles(wt.path),
    staleTime: STALE.fast,
  });

export const wtLockQuery = (wt: Pick<Worktree, "slug">) =>
  queryOptions({
    queryKey: qk.wt(wt.slug).lock(),
    queryFn: async (): Promise<Partial<LockMeta> | null> => lockStatus(wt.slug),
    staleTime: STALE.fast,
    // Poll more aggressively while a lock is held so "busy" phase text
    // updates without pressing `r`.
    refetchInterval: (query) => (query.state.data ? 2_000 : false),
  });

export const wtDeployQuery = (wt: Pick<Worktree, "slug" | "path">) =>
  queryOptions({
    queryKey: qk.wt(wt.slug).deploy(),
    queryFn: async (): Promise<boolean> => isOurStageDeployed(wt),
    staleTime: STALE.fast,
  });

export const wtMergedQuery = (wt: Pick<Worktree, "slug" | "branch">) =>
  queryOptions({
    queryKey: qk.wt(wt.slug).merged(),
    queryFn: async (): Promise<boolean> =>
      wt.branch ? branchIsMerged(wt.branch) : false,
    staleTime: STALE.mid,
  });

export const wtGoneQuery = (wt: Pick<Worktree, "slug" | "branch">) =>
  queryOptions({
    queryKey: qk.wt(wt.slug).gone(),
    queryFn: async (): Promise<boolean> =>
      wt.branch ? branchIsGone(wt.branch) : false,
    staleTime: STALE.mid,
  });

export const wtSyncQuery = (
  wt: Pick<Worktree, "slug" | "path">,
  effectiveBase?: string | null,
) => {
  const base = effectiveBase ?? `origin/${config.branch.base}`;
  return queryOptions({
    queryKey: qk.wt(wt.slug).sync(base),
    queryFn: async (): Promise<SyncState> => syncState(wt.path, base),
    staleTime: STALE.mid,
    ...KEEP_PREV,
  });
};

export const wtClaudeQuery = (wt: Pick<Worktree, "slug" | "path">) =>
  queryOptions({
    queryKey: qk.wt(wt.slug).claude(),
    queryFn: async (): Promise<ClaudeStatus> =>
      claudeStatus({ slug: wt.slug, path: wt.path }),
    staleTime: STALE.fast,
    // The session-tail slug sink is the primary trigger: it invalidates
    // this query the moment a live session's jsonl grows, so turn ends
    // and queue-count changes snap immediately. The interval only keeps
    // the *displayed age* ("2m ago") ticking and covers sessions the
    // tailer isn't watching — minute-granularity display needs no 5s
    // loop. State (working/waiting/abandoned/idle) is derived in the
    // row via `useClaudeSessionsForSlug`, which subscribes to
    // `tmuxSessionsQuery` (its own poll loop). A tmux state change
    // re-renders the row without rerunning this query.
    refetchInterval: 15_000,
  });

export const wtGitActivityQuery = (
  wt: Pick<Worktree, "slug" | "path" | "branch">,
  effectiveBase?: string | null,
) => {
  const base = effectiveBase ?? `origin/${config.branch.base}`;
  return queryOptions({
    queryKey: qk.wt(wt.slug).gitActivity(base),
    queryFn: async (): Promise<GitActivity> =>
      gitActivity({ path: wt.path, branch: wt.branch }, base),
    staleTime: STALE.mid,
    ...KEEP_PREV,
  });
};

/**
 * Rebase-conflict pre-flight: a `git merge-tree` dry-run of this
 * worktree's HEAD against its effective base (the parent branch for a
 * stacked slice, `origin/<trunk>` otherwise). Side-effect-free — never
 * touches the working tree. Keyed by base like `sync` / `gitActivity`;
 * the `.git/refs` watcher's `["wt"]` invalidation refetches it on any
 * commit / fetch / push, so it tracks reality without its own trigger.
 */
export const wtConflictQuery = (
  wt: Pick<Worktree, "slug" | "path">,
  effectiveBase?: string | null,
) => {
  const base = effectiveBase ?? `origin/${config.branch.base}`;
  return queryOptions({
    queryKey: qk.wt(wt.slug).conflict(base),
    queryFn: async (): Promise<MergeConflictProbe> =>
      mergeConflictProbe("HEAD", base, wt.path),
    staleTime: STALE.mid,
    ...KEEP_PREV,
  });
};

/**
 * Subject of the oldest commit on the branch — fallback title when
 * there's no PR yet. Cheap (one `git log`); short staleTime.
 */
export const wtFirstCommitQuery = (wt: Pick<Worktree, "slug" | "path">) =>
  queryOptions({
    queryKey: qk.wt(wt.slug).firstCommit(),
    queryFn: async (): Promise<string | null> => firstCommitSubject(wt.path),
    staleTime: STALE.mid,
  });
