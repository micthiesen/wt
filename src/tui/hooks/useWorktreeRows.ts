import { useEffect, useRef } from "react";
import { keepPreviousData, useQueries, useQuery, useQueryClient } from "@tanstack/react-query";

import type { ClaudeStatus } from "../../core/claude.ts";
import { config } from "../../core/config.ts";
import type { GitActivity } from "../../core/git-activity.ts";
import { pickPrForWorktree } from "../../core/github.ts";
import { lockAge, lockLabel } from "../../core/locks.ts";
import { latestLogFor } from "../../core/logs.ts";
import type { StackMap } from "../../core/stack.ts";
import { slugLabel } from "../../core/stage.ts";
import type { LockMeta, MergeQueueEntry, PullRequest, Status, Worktree } from "../../core/types.ts";
import { StatusKind } from "../../core/types.ts";
import type { SyncState } from "../../core/worktree.ts";
import { useGithub } from "../../state/hooks.ts";
import { qk } from "../../state/keys.ts";
import {
  aiSummaryQuery,
  archiveQuery,
  stackQuery,
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
  dirty: FieldState<readonly string[]>;
  lock: FieldState<Partial<LockMeta> | null>;
  deploy: FieldState<boolean>;
  merged: FieldState<boolean>;
  gone: FieldState<boolean>;
  sync: FieldState<SyncState>;
  claude: FieldState<ClaudeStatus>;
  gitActivity: FieldState<GitActivity>;
};

/**
 * Stack relationship for a worktree, with the resolved diff base.
 * Populated by three signals in priority order:
 *
 *   "commits"  — parent's tip is an ancestor of HEAD. Stack is in sync;
 *                three-dot diff against `branch` covers exactly this
 *                worktree's unique work. `diffBase === branch`.
 *   "patch-id" — parent rebased after the child branched, so its tip
 *                is no longer an ancestor of HEAD, but its commits (by
 *                patch-id) still appear in HEAD's history under
 *                different SHAs. `diffBase` is a SHA inside HEAD's
 *                history that skips the rebased-copy commits.
 *   "pr"       — neither commit nor patch-id signal fires, but the PR
 *                declares a non-trunk base. `diffBase === branch`; the
 *                diff may be inaccurate (no patch-id overlap means the
 *                two histories aren't actually related) but we honor
 *                the user's declared intent.
 *
 * Anything other than `"commits"` is "out of sync" — the consuming UI
 * surfaces a muted suffix so the user knows to rebase.
 *
 * `slug` is `null` for PR-base hits where the declared base isn't
 * another worktree in the list; the consumer can still use the diff
 * base for diffing but has no row to draw a UI hint to.
 */
export type StackedOn = {
  slug: string | null;
  branch: string;
  via: "commits" | "patch-id" | "pr";
  /** Ref or SHA to use for `git diff <diffBase>...HEAD`. */
  diffBase: string;
};

export type WorktreeRow = {
  wt: Worktree;
  fields: WorktreeFields;
  status: Status;
  pr?: PullRequest;
  mq?: MergeQueueEntry;
  /**
   * Resolved stack parent. `null` for trunk-targeted worktrees. Drives
   * the diff base for `wtDiffContextQuery` (so the AI summary describes
   * only what this PR adds on top of its parent) and the "↑" hint in
   * the worktree list when the parent is the row immediately above.
   */
  stackedOn: StackedOn | null;
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

function stackedOnEq(a: StackedOn | null, b: StackedOn | null): boolean {
  if (a === b) return true;
  if (!a || !b) return false;
  return (
    a.slug === b.slug &&
    a.branch === b.branch &&
    a.via === b.via &&
    a.diffBase === b.diffBase
  );
}

/**
 * Resolve `stackedOn` for a single worktree. Single source of truth for
 * the priority chain (commits → patch-id → pr → null) and the resolved
 * diff base — the row aggregator reads this to populate `row.stackedOn`,
 * and the details pane reads `row.stackedOn?.diffBase` directly so both
 * sites land queries in the same per-(slug, base) cache slot.
 *
 * `worktrees` is the active set; needed only to associate a PR's
 * declared base branch with a worktree slug for the UI hint.
 */
function resolveStackedOn(
  wt: Worktree,
  stackData: StackMap | undefined,
  pr: PullRequest | undefined,
  worktrees: readonly Worktree[],
): StackedOn | null {
  const fromStack = stackData?.[wt.slug];
  if (fromStack) {
    // Pre-patch-id cache entries lack `via` / `diffBase` but had a
    // valid `slug` + `branch`, which described a `"commits"` stack.
    // Fill the missing fields with that interpretation so a recently
    // upgraded process doesn't visibly flip stacked rows to "(pr)" or
    // trunk for one staleTime window.
    return {
      slug: fromStack.slug,
      branch: fromStack.branch,
      via: fromStack.via ?? "commits",
      diffBase: fromStack.diffBase ?? fromStack.branch,
    };
  }
  if (pr && pr.baseRefName && pr.baseRefName !== config.branch.base) {
    const parentWt = worktrees.find((w) => w.branch === pr.baseRefName);
    return {
      slug: parentWt?.slug ?? null,
      branch: pr.baseRefName,
      via: "pr",
      diffBase: pr.baseRefName,
    };
  }
  return null;
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
  const dirty = fields.dirty.data;
  if (dirty && dirty.length > 0) {
    // Single auto-regen file (default `sst-env.d.ts`) → label by name so
    // the user can tell at a glance that the dirt is just SST output and
    // not a real edit. Falls through to plain "dirty" for any other
    // mix.
    const regen = config.sst?.autoRegenPaths ?? [];
    if (dirty.length === 1 && regen.includes(dirty[0]!)) {
      return { kind: StatusKind.Dirty, label: dirty[0]! };
    }
    return { kind: StatusKind.Dirty, label: "dirty" };
  }
  return { kind: StatusKind.Clean, label: "clean" };
}

/**
 * Pick the row title with the `llm > pr > commit > slug` fallback
 * chain. The slug fallback is what guarantees the title field is
 * never empty: `slugLabel(...).rest` is the prettified tail (issue
 * ID stripped, dashes → spaces, first-letter caps); for slugs that
 * are *only* an issue prefix it falls back to the id and finally the
 * raw slug, so we always render something and the details pane keeps
 * a stable line count.
 */
function resolveTitle(
  slug: string,
  llmTitle: string | null,
  prTitle: string | null,
  commitTitle: string | null,
): { title: string; source: TitleSource } {
  if (llmTitle) return { title: llmTitle, source: "llm" };
  if (prTitle) return { title: prTitle, source: "pr" };
  if (commitTitle) return { title: commitTitle, source: "commit" };
  const { id, rest } = slugLabel(slug);
  return { title: rest || id || slug, source: "slug" };
}

/**
 * Section-aware sort for the active (non-archived) rows. Active rows
 * partition into:
 *   1. unsectioned (section === null) at the top.
 *   2. named sections in the order they appear in `sectionsOrder`
 *      (an explicit array maintained in state.json).
 * Within each bucket, rows sort by `state.order` ascending; unstated
 * entries float to the top (-Infinity) so brand-new worktrees always
 * land at the top of the unsectioned list. Sections not yet in
 * `sectionsOrder` (post-rename quirk, raw file edit) sort to the end
 * via `Number.MAX_SAFE_INTEGER` then alphabetically — display stays
 * stable until the index catches up on the next read.
 *
 * Returned as a fresh array; the caller is responsible for combining
 * with the archived rows and any rows-array identity stabilization.
 */
function sortActiveRows(
  active: WorktreeRow[],
  unsortedIndex: ReadonlyMap<string, number>,
  stateSlugs: Record<string, { order: number }>,
  sectionsOrder: readonly string[],
): WorktreeRow[] {
  const sectionRank = new Map<string, number>();
  for (let i = 0; i < sectionsOrder.length; i++) {
    sectionRank.set(sectionsOrder[i]!, i);
  }
  return active.slice().sort((a, b) => {
    const bucketA = a.section === null ? 0 : 1;
    const bucketB = b.section === null ? 0 : 1;
    if (bucketA !== bucketB) return bucketA - bucketB;
    if (a.section !== null && b.section !== null && a.section !== b.section) {
      const rankA = sectionRank.get(a.section) ?? Number.MAX_SAFE_INTEGER;
      const rankB = sectionRank.get(b.section) ?? Number.MAX_SAFE_INTEGER;
      if (rankA !== rankB) return rankA - rankB;
      return a.section.localeCompare(b.section);
    }
    const orderA = stateSlugs[a.wt.slug]?.order ?? -Infinity;
    const orderB = stateSlugs[b.wt.slug]?.order ?? -Infinity;
    if (orderA !== orderB) return orderA - orderB;
    return (unsortedIndex.get(a.wt.slug) ?? 0) - (unsortedIndex.get(b.wt.slug) ?? 0);
  });
}

/**
 * Watch the set of slugs that currently hold a lock. When a slug
 * transitions from held → released, invalidate the worktree list so
 * a destroyed slug drops promptly instead of lingering as a stale
 * merged/gone candidate until its 15s staleTime expires.
 *
 * The signal arrives as a JSON-encoded sorted slug list so React
 * compares by string identity (cheap, stable). A foreign-author slug
 * that happens to contain a delimiter character can't smear set
 * membership because we go through JSON.
 */
function useLockReleasedInvalidator(lockedSig: string): void {
  const qc = useQueryClient();
  const prevLockedRef = useRef<Set<string>>(new Set());
  useEffect(() => {
    const curr = new Set<string>(JSON.parse(lockedSig) as string[]);
    const prev = prevLockedRef.current;
    let released = false;
    for (const slug of prev) {
      if (!curr.has(slug)) {
        released = true;
        break;
      }
    }
    prevLockedRef.current = curr;
    if (released) {
      void qc.invalidateQueries({ queryKey: qk.worktrees() });
    }
  }, [lockedSig, qc]);
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

  // Stack detection runs once for the full set; the result is shared
  // across all per-worktree consumers below (effective diff base, the
  // `stackedOn` field on rows). Keyed by branch list so worktree churn
  // re-triggers; SHA drift inside a fixed set is caught by staleTime.
  const stack = useQuery({
    ...stackQuery(wtList.data ?? []),
    enabled: !!wtList.data && wtList.data.length > 0,
  });

  // Per-worktree PR lookup — used both for `stackedOn` resolution
  // below and for the row's `pr` field further down. Hoisted so the
  // GitHub map is only walked once per worktree per render.
  const prsByIndex = worktrees.map((wt) =>
    pickPrForWorktree(wt, github.data?.prs),
  );

  // Resolve `stackedOn` once per render, then derive the diff base from
  // it. Single source of truth for the commits → patch-id → pr → null
  // priority chain — every consumer (sync/git-activity/diff-context
  // queries here, the diff query in `details.tsx`, the row UI hint)
  // reads through `row.stackedOn`, so they all land queries in the same
  // per-(slug, base) cache slot.
  const stackedOnByIndex = worktrees.map((wt, i) =>
    resolveStackedOn(wt, stack.data, prsByIndex[i], worktrees),
  );
  const bases = stackedOnByIndex.map((s) => s?.diffBase ?? null);

  const queries = worktrees.flatMap((wt, i) => [
    wtDirtyQuery(wt),
    wtLockQuery(wt),
    wtDeployQuery(wt),
    wtMergedQuery(wt),
    wtGoneQuery(wt),
    wtSyncQuery(wt, bases[i]!),
    wtClaudeQuery(wt),
    wtGitActivityQuery(wt, bases[i]!),
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

  // Lock-released → invalidate worktrees. Effect lives in
  // `useLockReleasedInvalidator`; the body of this hook just produces
  // the signal.
  const lockedSlugs = worktrees
    .filter((_, i) => busyByIndex[i])
    .map((w) => w.slug)
    .sort();
  useLockReleasedInvalidator(JSON.stringify(lockedSlugs));

  const diffResults = useQueries({
    queries: worktrees.map((wt, i) => ({
      ...wtDiffContextQuery(wt, bases[i]!),
      enabled: !busyByIndex[i],
    })),
  });

  // Hash-keyed AI summary: a diff change re-keys the query, the
  // observer cache-misses for the new hash, and `keepPreviousData`
  // keeps the prior summary on screen while the new fetch runs. No
  // mismatch effect required — the cache key change *is* the trigger.
  const aiResults = useQueries({
    queries: worktrees.map((wt, i) => {
      const ctx = diffResults[i]?.data ?? null;
      return {
        ...aiSummaryQuery(wt.slug, ctx),
        enabled: aiEnabled && !busyByIndex[i] && !!ctx,
        placeholderData: keepPreviousData,
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
      dirty: reuseField(prev?.fields.dirty, toFieldState(fieldArr[0] as FieldState<readonly string[]>)),
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
    const pr = prsByIndex[i];
    const mq = wt.branch ? github.data?.mergeQueue?.[wt.branch] : undefined;
    const stackedOn = stackedOnByIndex[i] ?? null;
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
    const llmTitle = aiResults[i]?.data?.title ?? null;
    const llmBrief = aiResults[i]?.data?.brief ?? null;
    const prTitle = pr?.title ?? null;
    const commitTitle = firstCommitResults[i]?.data ?? null;
    const { title, source: titleSource } = resolveTitle(
      wt.slug,
      llmTitle,
      prTitle,
      commitTitle,
    );
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
      prev.section === section &&
      stackedOnEq(prev.stackedOn, stackedOn)
    ) {
      return prev;
    }
    // Reuse prev's stackedOn reference when value-equal so memoized
    // children downstream skip the work.
    const stackedOnOut = prev && stackedOnEq(prev.stackedOn, stackedOn)
      ? prev.stackedOn
      : stackedOn;
    const next: WorktreeRow = {
      wt,
      fields,
      status,
      pr,
      mq,
      stackedOn: stackedOnOut,
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

  // Section-aware sort lives in `sortActiveRows`. Archived rows are
  // flat at the bottom in original list order — the archive divider is
  // a hard visual break, secondary grouping there would be noise.
  const listIndexOf = new Map<string, number>();
  for (let i = 0; i < unsorted.length; i++) {
    listIndexOf.set(unsorted[i]!.wt.slug, i);
  }
  const sectionsOrder = wtState.data?.sectionsOrder ?? [];
  const active = sortActiveRows(
    unsorted.filter((r) => !r.archived),
    listIndexOf,
    stateSlugs,
    sectionsOrder,
  );
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
