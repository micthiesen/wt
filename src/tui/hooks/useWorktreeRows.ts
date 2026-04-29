import { useQueries, useQuery } from "@tanstack/react-query";

import type { ClaudeStatus } from "../../core/claude.ts";
import { config } from "../../core/config.ts";
import type { GitActivity } from "../../core/git-activity.ts";
import { pickPrForWorktree } from "../../core/github.ts";
import { lockAge, lockLabel } from "../../core/locks.ts";
import { latestLogFor } from "../../core/logs.ts";
import type { LockMeta, MergeQueueEntry, PullRequest, Status, Worktree } from "../../core/types.ts";
import { StatusKind } from "../../core/types.ts";
import type { SyncState } from "../../core/worktree.ts";
import { useGithub } from "../../state/hooks.ts";
import {
  aiSummaryQuery,
  archiveQuery,
  worktreesQuery,
  wtClaudeQuery,
  wtDeployQuery,
  wtDiffContextQuery,
  wtDirtyQuery,
  wtFirstCommitQuery,
  wtGitActivityQuery,
  wtGoneQuery,
  wtLockQuery,
  wtMergedQuery,
  wtSyncQuery,
} from "../../state/queries.ts";

/**
 * Where the row's resolved title came from, in fallback priority. The
 * details pane renders this as a muted suffix so a stale PR title vs.
 * a freshly LLM-generated one is obvious at a glance.
 */
export type TitleSource = "llm" | "pr" | "commit";

export type FieldState<T> = {
  data: T | undefined;
  isStale: boolean;
  isFetching: boolean;
  isLoading: boolean;
  /** Populated once retries are exhausted; cleared when a refetch starts. */
  error: Error | null;
};

export type WorktreeFields = {
  dirty: FieldState<boolean>;
  lock: FieldState<Partial<LockMeta> | null>;
  deploy: FieldState<boolean>;
  merged: FieldState<boolean>;
  gone: FieldState<boolean>;
  sync: FieldState<SyncState>;
  claude: FieldState<ClaudeStatus>;
  gitActivity: FieldState<GitActivity>;
};

export type WorktreeRow = {
  wt: Worktree;
  fields: WorktreeFields;
  status: Status;
  pr?: PullRequest;
  mq?: MergeQueueEntry;
  anyFetching: boolean;
  archived: boolean;
  /**
   * Resolved title with `llm > pr > commit` fallback. Both the list
   * row label and the details-pane title bar read this so they stay in
   * sync. Null when no source has produced anything yet (AI still
   * generating, no PR, branch with no commits) — in which case the
   * list falls back to a slug-derived label and the details pane
   * shows nothing.
   */
  title: string | null;
  titleSource: TitleSource | null;
};

const FIELD_ORDER = [
  "dirty",
  "lock",
  "deploy",
  "merged",
  "gone",
  "sync",
  "claude",
  "gitActivity",
] as const;

export type WorktreeRowsResult = {
  rows: WorktreeRow[];
  isLoading: boolean;
};

function toFieldState<T>(r: {
  data: T | undefined;
  isStale: boolean;
  isFetching: boolean;
  isLoading: boolean;
  error: Error | null;
}): FieldState<T> {
  return {
    data: r.data,
    isStale: r.isStale,
    isFetching: r.isFetching,
    isLoading: r.isLoading,
    error: r.error,
  };
}

function deriveStatus(
  wt: Worktree,
  fields: WorktreeFields,
): Status {
  const lock = fields.lock.data;
  if (lock && Object.keys(lock).length > 0) {
    return {
      kind: StatusKind.Busy,
      label: lockLabel(lock),
      age: lockAge(lock) ?? undefined,
      log: latestLogFor(wt.slug) ?? undefined,
      pid: lock.pid,
      op: lock.op,
    };
  }
  if (fields.gone.data) {
    return { kind: StatusKind.Gone, label: "gone (squash-merged or deleted)" };
  }
  if (fields.merged.data) {
    return { kind: StatusKind.Merged, label: "merged into origin/main" };
  }
  if (fields.dirty.data) {
    return { kind: StatusKind.Dirty, label: "dirty" };
  }
  return { kind: StatusKind.Clean, label: "clean" };
}

/**
 * Fetches the worktree list and, in a single `useQueries` batch, every
 * per-property field for every non-main worktree. Results are stitched
 * back into a row per worktree with a derived `Status`.
 */
export function useWorktreeRows(): WorktreeRowsResult {
  const wtList = useQuery(worktreesQuery());
  const github = useGithub();
  const archive = useQuery(archiveQuery());
  const archivedSet = new Set(archive.data ?? []);

  const worktrees = (wtList.data ?? []).filter((w) => !w.isMain);

  const queries = worktrees.flatMap((wt) => [
    wtDirtyQuery(wt),
    wtLockQuery(wt),
    wtDeployQuery(wt),
    wtMergedQuery(wt),
    wtGoneQuery(wt),
    wtSyncQuery(wt),
    wtClaudeQuery(wt),
    wtGitActivityQuery(wt),
  ]);

  // `combine` runs on every render with the latest results — cheap since
  // `useQueries` caches identity.
  const results = useQueries({ queries });

  // Diff context + AI summary observers for every worktree, so the list
  // panel can render LLM-generated titles next to each row. The cache is
  // content-addressed and persisted, so steady-state these are no-op
  // hits; only new/changed worktrees trigger an LM Studio call. Gated on
  // the lock state from the batch above so we don't race a destroying
  // worktree's git state.
  const aiEnabled = !!config.ai;
  const busyByIndex = worktrees.map((_, i) => {
    const lock = results[i * FIELD_ORDER.length + 1]?.data as
      | Partial<LockMeta>
      | null
      | undefined;
    return !!(lock && Object.keys(lock).length > 0);
  });

  const diffResults = useQueries({
    queries: worktrees.map((wt, i) => ({
      ...wtDiffContextQuery(wt),
      enabled: !busyByIndex[i],
    })),
  });

  const aiResults = useQueries({
    queries: worktrees.map((wt, i) => {
      const ctx = diffResults[i]?.data ?? null;
      return {
        ...aiSummaryQuery(wt.slug, ctx),
        enabled: aiEnabled && !busyByIndex[i] && !!ctx,
      };
    }),
  });

  // First-commit subject — non-AI fallback for the title resolution
  // chain. Cheap (one `git log`); paused only while busy so we don't
  // race a destroying worktree's git state.
  const firstCommitResults = useQueries({
    queries: worktrees.map((wt, i) => ({
      ...wtFirstCommitQuery(wt),
      enabled: !busyByIndex[i],
    })),
  });

  const unsorted: WorktreeRow[] = worktrees.map((wt, i) => {
    const base = i * FIELD_ORDER.length;
    const fieldArr = FIELD_ORDER.map((_, j) => results[base + j]!);
    const fields: WorktreeFields = {
      dirty: toFieldState(fieldArr[0] as FieldState<boolean>),
      lock: toFieldState(fieldArr[1] as FieldState<Partial<LockMeta> | null>),
      deploy: toFieldState(fieldArr[2] as FieldState<boolean>),
      merged: toFieldState(fieldArr[3] as FieldState<boolean>),
      gone: toFieldState(fieldArr[4] as FieldState<boolean>),
      sync: toFieldState(fieldArr[5] as FieldState<SyncState>),
      claude: toFieldState(fieldArr[6] as FieldState<ClaudeStatus>),
      gitActivity: toFieldState(fieldArr[7] as FieldState<GitActivity>),
    };
    const status = deriveStatus(wt, fields);
    const pr = pickPrForWorktree(wt, github.data?.prs);
    const mq = wt.branch ? github.data?.mergeQueue?.[wt.branch] : undefined;
    // Include the combined GitHub fetch that feeds the details pane
    // so the row glyph lights up whenever anything visible for this
    // worktree is refreshing — not just the per-worktree fields. That
    // one fetch covers all rows at once, so every row's glyph flashes
    // together during e.g. a refresh; intentional, since each row's
    // details pane would show the PR/merge-queue spinner.
    const anyFetching =
      fieldArr.some((r) => r.isFetching) || github.isFetching;
    const archived = archivedSet.has(wt.slug);
    const llmTitle = (aiResults[i]?.data?.title as string | undefined) ?? null;
    const prTitle = pr?.title ?? null;
    const commitTitle = (firstCommitResults[i]?.data as string | null | undefined) ?? null;
    let title: string | null = null;
    let titleSource: TitleSource | null = null;
    if (llmTitle) {
      title = llmTitle;
      titleSource = "llm";
    } else if (prTitle) {
      title = prTitle;
      titleSource = "pr";
    } else if (commitTitle) {
      title = commitTitle;
      titleSource = "commit";
    }
    return { wt, fields, status, pr, mq, anyFetching, archived, title, titleSource };
  });

  // Stable partition: active first (preserves git worktree order),
  // archived second (so `j/k` moves through both seamlessly).
  const rows: WorktreeRow[] = [
    ...unsorted.filter((r) => !r.archived),
    ...unsorted.filter((r) => r.archived),
  ];

  return {
    rows,
    isLoading: wtList.isLoading,
  };
}
