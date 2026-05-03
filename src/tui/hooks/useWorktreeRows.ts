import { useEffect, useRef } from "react";
import { useQueries, useQuery, useQueryClient } from "@tanstack/react-query";

import type { ClaudeStatus } from "../../core/claude.ts";
import { config } from "../../core/config.ts";
import type { GitActivity } from "../../core/git-activity.ts";
import { pickPrForWorktree } from "../../core/github.ts";
import { lockAge, lockLabel } from "../../core/locks.ts";
import { latestLogFor } from "../../core/logs.ts";
import { slugLabel } from "../../core/stage.ts";
import type { LockMeta, MergeQueueEntry, PullRequest, Status, Worktree } from "../../core/types.ts";
import { StatusKind } from "../../core/types.ts";
import type { SyncState } from "../../core/worktree.ts";
import { useGithub } from "../../state/hooks.ts";
import { qk } from "../../state/keys.ts";
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
  wtStateQuery,
  wtSyncQuery,
} from "../../state/queries.ts";

/**
 * Where the row's resolved title came from, in fallback priority. The
 * details pane renders this as a muted suffix so a stale PR title vs.
 * a freshly LLM-generated one is obvious at a glance. `slug` is the
 * terminal fallback — the prettified slug is always available, so a
 * row's title field is never empty.
 */
export type TitleSource = "llm" | "pr" | "commit" | "slug";

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
   * Resolved title with `llm > pr > commit > slug` fallback. Both the
   * list row label and the details-pane title bar read this so they
   * stay in sync. Always non-empty — `slugLabel` produces a prettified
   * fallback for any worktree, so consumers never need to check for
   * null.
   */
  title: string;
  titleSource: TitleSource;
  /**
   * Ultra-short LLM-authored label for the worktree list, where space
   * after the issue ID and badge cluster is tight. Null when the AI
   * source hasn't produced a summary yet; the list panel falls back to
   * `title` in that case.
   */
  brief: string | null;
  /**
   * User-assigned section name from `state.json`, or `null` for the
   * unsectioned bucket at the top of the list. Persisted across the
   * archived flag — restoring an archived worktree drops it back into
   * the same named section it was in before.
   */
  section: string | null;
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
  const qc = useQueryClient();
  const wtList = useQuery(worktreesQuery());
  const github = useGithub();
  const archive = useQuery(archiveQuery());
  const wtState = useQuery(wtStateQuery());
  const archivedSet = new Set(archive.data ?? []);
  const stateSlugs = wtState.data?.slugs ?? {};
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
        ...aiSummaryQuery(qc, wt.slug, ctx),
        enabled: aiEnabled && !busyByIndex[i] && !!ctx,
      };
    }),
  });

  // Slug-keyed cache means the queryFn won't re-run on its own when
  // the diff changes — the queryKey is unchanged. Detect drift here
  // and invalidate; the observer's refetch picks up the new closure
  // (with the new ctx hash) and the queryFn either hits the memo or
  // calls LM Studio. During the refetch, `data` keeps the prior
  // summary visible, which is the whole point of slug-keying.
  const mismatchSig = worktrees
    .map((wt, i) => {
      const dataHash = aiResults[i]?.data?.hash;
      const ctxHash = diffResults[i]?.data?.hash;
      return dataHash && ctxHash && dataHash !== ctxHash ? wt.slug : "";
    })
    .filter(Boolean)
    .join("|");
  useEffect(() => {
    if (!mismatchSig) return;
    for (const slug of mismatchSig.split("|")) {
      void qc.invalidateQueries({ queryKey: qk.aiSummary(slug) });
    }
  }, [mismatchSig, qc]);

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
    const section = stateSlugs[wt.slug]?.section ?? null;
    const llmTitle = (aiResults[i]?.data?.title as string | undefined) ?? null;
    const llmBrief = (aiResults[i]?.data?.brief as string | undefined) ?? null;
    const prTitle = pr?.title ?? null;
    const commitTitle = (firstCommitResults[i]?.data as string | null | undefined) ?? null;
    let title: string;
    let titleSource: TitleSource;
    if (llmTitle) {
      title = llmTitle;
      titleSource = "llm";
    } else if (prTitle) {
      title = prTitle;
      titleSource = "pr";
    } else if (commitTitle) {
      title = commitTitle;
      titleSource = "commit";
    } else {
      // Slug-derived fallback. `slugLabel(...).rest` is the prettified
      // tail (issue ID stripped, dashes → spaces, first-letter caps).
      // Empty rest happens for slugs that are only an issue prefix —
      // fall back to the id, then the raw slug, so we always render
      // *something* and the details pane keeps a stable line count.
      const { id, rest } = slugLabel(wt.slug);
      title = rest || id || wt.slug;
      titleSource = "slug";
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
      prev.titleSource === titleSource &&
      prev.brief === llmBrief &&
      prev.section === section
    ) {
      return prev;
    }
    const next: WorktreeRow = {
      wt,
      fields,
      status,
      pr,
      mq,
      anyFetching,
      archived,
      title,
      titleSource,
      brief: llmBrief,
      section,
    };
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

  // Section-aware sort. Active rows partition into:
  //   1. unsectioned (section === null)
  //   2. named sections, ordered by their position in `sectionsOrder`
  //      (an explicit array maintained in state.json, appended on
  //      first encounter and pruned on emptiness).
  // Within each bucket, rows sort by `state.order` ascending; unstated
  // entries (no state row yet) float to the top (-Infinity) so brand-
  // new worktrees always land at the top of the unsectioned list.
  // Archived rows are flat at the bottom in original list order — the
  // archive divider is a hard visual break, secondary grouping there
  // would be noise (and the user explicitly asked for a flat list).
  const listIndexOf = new Map<string, number>();
  for (let i = 0; i < unsorted.length; i++) {
    listIndexOf.set(unsorted[i]!.wt.slug, i);
  }
  const sectionsOrder = wtState.data?.sectionsOrder ?? [];
  const sectionRank = new Map<string, number>();
  for (let i = 0; i < sectionsOrder.length; i++) {
    sectionRank.set(sectionsOrder[i]!, i);
  }
  const active = unsorted.filter((r) => !r.archived).slice().sort((a, b) => {
    const bucketA = a.section === null ? 0 : 1;
    const bucketB = b.section === null ? 0 : 1;
    if (bucketA !== bucketB) return bucketA - bucketB;
    if (a.section !== null && b.section !== null && a.section !== b.section) {
      // Sections not yet in `sectionsOrder` (post-rename quirk, raw
      // file edit) sort to the end via `Number.MAX_SAFE_INTEGER`,
      // then alphabetically — keeps display stable until the index
      // catches up on the next read.
      const rankA = sectionRank.get(a.section) ?? Number.MAX_SAFE_INTEGER;
      const rankB = sectionRank.get(b.section) ?? Number.MAX_SAFE_INTEGER;
      if (rankA !== rankB) return rankA - rankB;
      return a.section.localeCompare(b.section);
    }
    const orderA = stateSlugs[a.wt.slug]?.order ?? -Infinity;
    const orderB = stateSlugs[b.wt.slug]?.order ?? -Infinity;
    if (orderA !== orderB) return orderA - orderB;
    return (listIndexOf.get(a.wt.slug) ?? 0) - (listIndexOf.get(b.wt.slug) ?? 0);
  });
  const archived = unsorted.filter((r) => r.archived);
  const nextRows: WorktreeRow[] = [...active, ...archived];
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
