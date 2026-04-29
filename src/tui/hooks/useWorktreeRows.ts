import { useRef } from "react";
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

/**
 * Reuse the previous `FieldState` reference when every observable
 * property is identity-equal. Lets memoized children (and the row-level
 * identity check below) skip work whenever nothing actually changed.
 */
function reuseField<T>(
  prev: FieldState<T> | undefined,
  next: FieldState<T>,
): FieldState<T> {
  if (
    prev &&
    prev.data === next.data &&
    prev.isStale === next.isStale &&
    prev.isFetching === next.isFetching &&
    prev.isLoading === next.isLoading &&
    prev.error === next.error
  ) {
    return prev;
  }
  return next;
}

function statusEq(a: Status, b: Status): boolean {
  return (
    a.kind === b.kind &&
    a.label === b.label &&
    a.age === b.age &&
    a.log === b.log &&
    a.pid === b.pid &&
    a.op === b.op
  );
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
  // Keyed by slug; lets us return the same `WorktreeRow` reference
  // across renders when nothing observable has changed. Without this,
  // every poll-driven refresh produces all-new row identities and
  // forces every downstream `useMemo` / `React.memo` to re-run.
  const rowCache = useRef<Map<string, WorktreeRow>>(new Map());
  const rowsRef = useRef<WorktreeRow[]>([]);

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
    const prev = rowCache.current.get(wt.slug);
    const fields: WorktreeFields = {
      dirty: reuseField(prev?.fields.dirty, toFieldState(fieldArr[0] as FieldState<boolean>)),
      lock: reuseField(prev?.fields.lock, toFieldState(fieldArr[1] as FieldState<Partial<LockMeta> | null>)),
      deploy: reuseField(prev?.fields.deploy, toFieldState(fieldArr[2] as FieldState<boolean>)),
      merged: reuseField(prev?.fields.merged, toFieldState(fieldArr[3] as FieldState<boolean>)),
      gone: reuseField(prev?.fields.gone, toFieldState(fieldArr[4] as FieldState<boolean>)),
      sync: reuseField(prev?.fields.sync, toFieldState(fieldArr[5] as FieldState<SyncState>)),
      claude: reuseField(prev?.fields.claude, toFieldState(fieldArr[6] as FieldState<ClaudeStatus>)),
      gitActivity: reuseField(prev?.fields.gitActivity, toFieldState(fieldArr[7] as FieldState<GitActivity>)),
    };
    const nextStatus = deriveStatus(wt, fields);
    const status = prev && statusEq(prev.status, nextStatus) ? prev.status : nextStatus;
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
    // After per-field reuse above, identity-equality on each `fields.X`,
    // `status`, `pr`, `mq` plus primitives is sufficient — anything
    // observable changing produces a fresh reference at one of those
    // levels, which falls through to a new row.
    if (
      prev &&
      prev.wt === wt &&
      prev.fields.dirty === fields.dirty &&
      prev.fields.lock === fields.lock &&
      prev.fields.deploy === fields.deploy &&
      prev.fields.merged === fields.merged &&
      prev.fields.gone === fields.gone &&
      prev.fields.sync === fields.sync &&
      prev.fields.claude === fields.claude &&
      prev.fields.gitActivity === fields.gitActivity &&
      prev.status === status &&
      prev.pr === pr &&
      prev.mq === mq &&
      prev.anyFetching === anyFetching &&
      prev.archived === archived &&
      prev.title === title &&
      prev.titleSource === titleSource
    ) {
      return prev;
    }
    const next: WorktreeRow = { wt, fields, status, pr, mq, anyFetching, archived, title, titleSource };
    rowCache.current.set(wt.slug, next);
    return next;
  });

  // Drop cache entries for slugs that no longer exist so the map
  // doesn't grow unboundedly across the session.
  if (rowCache.current.size > worktrees.length) {
    const live = new Set(worktrees.map((w) => w.slug));
    for (const slug of rowCache.current.keys()) {
      if (!live.has(slug)) rowCache.current.delete(slug);
    }
  }

  // Stable partition: active first (preserves git worktree order),
  // archived second (so `j/k` moves through both seamlessly). Reuse
  // the prior array reference when every row in order matches —
  // otherwise consumer memos keyed on `rows` invalidate every render.
  const nextRows: WorktreeRow[] = [
    ...unsorted.filter((r) => !r.archived),
    ...unsorted.filter((r) => r.archived),
  ];
  const prevRows = rowsRef.current;
  let rowsUnchanged = prevRows.length === nextRows.length;
  if (rowsUnchanged) {
    for (let i = 0; i < nextRows.length; i++) {
      if (prevRows[i] !== nextRows[i]) {
        rowsUnchanged = false;
        break;
      }
    }
  }
  const rows = rowsUnchanged ? prevRows : (rowsRef.current = nextRows);

  return {
    rows,
    isLoading: wtList.isLoading,
  };
}
