import { useEffect, useMemo, useRef } from "react";
import { keepPreviousData, useQueries, useQuery, useQueryClient } from "@tanstack/react-query";

import type { ClaudeStatus } from "../../core/claude.ts";
import { config } from "../../core/config.ts";
import type { MergeConflictProbe } from "../../core/git.ts";
import type { GitActivity } from "../../core/git-activity.ts";
import { pickPrForWorktree } from "../../core/github.ts";
import { lockAge, lockLabel } from "../../core/locks.ts";
import { latestLogFor } from "../../core/logs.ts";
import {
  buildStackIndex,
  type SpinePos,
  type StackIndexEntry,
} from "../../core/stack-layout.ts";
import { slugLabel } from "../../core/stage.ts";
import type { LockMeta, MergeQueueEntry, PullRequest, Status, Worktree } from "../../core/types.ts";
import { StatusKind } from "../../core/types.ts";
import type { SyncState } from "../../core/worktree.ts";
import {
  GROUP_INBOX,
  STACK_SECTION_PREFIX,
  stackSectionKey,
  type WtState,
} from "../../core/wtstate.ts";
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
  wtConflictQuery,
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
  conflict: FieldState<MergeConflictProbe>;
};

/**
 * Stack relationship for a worktree, with the resolved diff base.
 *
 * Two explicit sources, no inference (no reflog / PR-base guessing):
 * `"stack"` — the MANIFEST (`wtState.stacks`): a worktree whose branch
 * matches a manifest slice with a non-trunk parent stacks on that
 * parent; `"fork"` — the per-slug `baseBranch` recorded by
 * `wt new --base` for worktrees that aren't (yet) manifest slices.
 * A manifest slice always resolves from the manifest alone — a lane
 * root renders flat even if a vestigial fork base is recorded.
 *
 * `slug` is `null` when the parent branch isn't materialized as a live
 * worktree; the consumer can still use the diff base for diffing but
 * has no row to draw a UI hint to.
 */
export type StackedOn = {
  slug: string | null;
  branch: string;
  via: "stack" | "fork";
  /** Ref to use for `git diff <diffBase>...HEAD`. */
  diffBase: string;
};

/**
 * Placement of a managed slice within its stack, derived from the
 * manifest layout. Drives the list's tree spine + ordinal. `null` for
 * any worktree that isn't a manifest slice.
 */
export type StackRowInfo = {
  stackId: string;
  /** Stack ordinal (1-based) shown in the row gutter. */
  ordinal: number;
  /** Spine position → connector glyph (single / first ┌ / middle ├ / last └ / fork ┯). */
  pos: SpinePos;
  /** Parallel-lane index → connector color (0 = main spine, dim). */
  lane: number;
  /** Depth from the lane root (root = 0). */
  depth: number;
  /** Display index within the stack (lane order, then depth). */
  index: number;
  /**
   * True for the holistic-origin worktree pinned at the bottom of its
   * stack section, not a slice. It carries no ordinal and renders with a
   * distinct dim glyph so it reads as "the source this stack was carved
   * from", kept around until the user `wt rm`s it post-split.
   */
  isHolistic: boolean;
};

export type WorktreeRow = {
  wt: Worktree;
  fields: WorktreeFields;
  status: Status;
  pr?: PullRequest;
  /**
   * GitHub merge-queue entry for this worktree's branch, when the PR is
   * enqueued. Carries the queue position + state. Absent when the
   * branch isn't in the merge queue. Keyed off the github fetch's
   * `mergeQueue` map by branch.
   */
  mq?: MergeQueueEntry;
  /**
   * Resolved stack parent (manifest-derived). `null` for trunk-targeted
   * worktrees. Drives the diff base for `wtDiffContextQuery` (so the AI
   * summary describes only what this slice adds on top of its parent);
   * the relationship is shown by the tree spine in the list (see
   * `row.stack`), not a separate badge.
   */
  stackedOn: StackedOn | null;
  /**
   * Placement within a managed stack (manifest-derived), or `null` when
   * this worktree isn't a slice of any manifest. When set, `section` is
   * the stack's synthetic key and `sectionIsStack` is true.
   */
  stack: StackRowInfo | null;
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
   * Effective section. A managed slice's section is the synthetic stack
   * key (`STACK_SECTION_PREFIX + stackId`), which overrides any manual
   * placement; otherwise it's the slug's stored `slugs[slug].section`.
   * `null` means the unsectioned inbox.
   */
  section: string | null;
  /**
   * True when `section` is a manifest-driven stack section. Drives the
   * spine rendering in the list pane and the J/K/move-into refusals in
   * the action layer (stack order is locked to the manifest).
   */
  sectionIsStack: boolean;
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
  "conflict",
] as const;

const EMPTY_STATE_SLUGS: WtState["slugs"] = {};

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

type QuerySnapshot<T = unknown> = {
  data: T | undefined;
  isStale: boolean;
  isFetching: boolean;
  isLoading: boolean;
  error: Error | null;
};

function combineQuerySnapshots(
  results: readonly QuerySnapshot[],
): readonly QuerySnapshot[] {
  return results.map((r) => ({
    data: r.data,
    isStale: r.isStale,
    isFetching: r.isFetching,
    isLoading: r.isLoading,
    error: r.error,
  }));
}

function combineQueryData<T>(
  results: readonly { data: T | undefined }[],
): readonly (T | undefined)[] {
  return results.map((r) => r.data);
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

// Synthetic stack section keys + the inbox sentinel live in
// `core/wtstate.ts` (the owner of the unified group order); re-exported
// here so the TUI keeps one import site for row/section plumbing.
export { GROUP_INBOX, STACK_SECTION_PREFIX, stackSectionKey } from "../../core/wtstate.ts";

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

function stackInfoEq(a: StackRowInfo | null, b: StackRowInfo | null): boolean {
  if (a === b) return true;
  if (!a || !b) return false;
  return (
    a.stackId === b.stackId &&
    a.ordinal === b.ordinal &&
    a.pos === b.pos &&
    a.lane === b.lane &&
    a.depth === b.depth &&
    a.index === b.index &&
    a.isHolistic === b.isHolistic
  );
}

/**
 * Resolve `stackedOn` (the diff base) for a worktree. A managed slice
 * resolves from its manifest layout entry alone: a non-trunk parent
 * diffs against that parent's branch, a lane root renders flat (null).
 * A non-slice worktree falls back to its recorded fork base
 * (`wt new --base` → slug-state `baseBranch`). The parent's live
 * worktree slug (when materialized) lets the list draw the relationship.
 */
function resolveStackedOn(
  entry: StackIndexEntry | undefined,
  worktrees: readonly Worktree[],
  forkBase: string | undefined,
): StackedOn | null {
  if (entry) {
    const parentBranch = entry.node.parentBranch;
    if (!parentBranch || parentBranch === config.branch.base) return null;
    const parentWt = worktrees.find((w) => w.branch === parentBranch);
    return {
      slug: parentWt?.slug ?? null,
      branch: parentBranch,
      via: "stack",
      diffBase: parentBranch,
    };
  }
  if (!forkBase || forkBase === config.branch.base) return null;
  const parentWt = worktrees.find((w) => w.branch === forkBase);
  return {
    slug: parentWt?.slug ?? null,
    branch: forkBase,
    via: "fork",
    diffBase: forkBase,
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
 * Section-aware sort for the active (non-archived) rows. Display order
 * is one unified ranked list of GROUPS — stack sections, the
 * unsectioned inbox (`GROUP_INBOX` sentinel), and manual named
 * sections — exactly as they appear in `sectionsOrder` (readWtState
 * self-heals it: new stacks float to the top, dead groups drop, and a
 * pre-unification file is seeded with the legacy stacks/inbox/manual
 * layout). Within each group, rows sort by `state.order` ascending
 * (stack rows by manifest layout index via `effectiveOrders`); unstated
 * entries float to the top (-Infinity) so brand-new worktrees always
 * land at the top of the inbox. Groups not yet ranked (a freshly
 * created stack mid-render, post-rename quirk) degrade predictably:
 * stack keys sort to the front (where new stacks live), manual names
 * to the end, then alphabetically — display stays stable until the
 * self-heal catches up on the next read.
 *
 * Returned as a fresh array; the caller is responsible for combining
 * with the archived rows and any rows-array identity stabilization.
 */
function sortActiveRows(
  active: WorktreeRow[],
  unsortedIndex: ReadonlyMap<string, number>,
  effectiveOrders: ReadonlyMap<string, number>,
  sectionsOrder: readonly string[],
): WorktreeRow[] {
  const rank = new Map<string, number>();
  for (let i = 0; i < sectionsOrder.length; i++) {
    rank.set(sectionsOrder[i]!, i);
  }
  const groupOf = (r: WorktreeRow): string => r.section ?? GROUP_INBOX;
  const rankOf = (g: string): number =>
    rank.get(g) ??
    (g.startsWith(STACK_SECTION_PREFIX) ? -1 : Number.MAX_SAFE_INTEGER);
  return active.slice().sort((a, b) => {
    const groupA = groupOf(a);
    const groupB = groupOf(b);
    if (groupA !== groupB) {
      const rankA = rankOf(groupA);
      const rankB = rankOf(groupB);
      if (rankA !== rankB) return rankA - rankB;
      return groupA.localeCompare(groupB);
    }
    const orderA = effectiveOrders.get(a.wt.slug) ?? -Infinity;
    const orderB = effectiveOrders.get(b.wt.slug) ?? -Infinity;
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
      // Destroys (and `init`) mutate state.json from the child process
      // — refresh the wtState query so the row aggregator sees those
      // mutations (slug-state reap, manifest updates) without waiting
      // for the staleTime to expire.
      void qc.invalidateQueries({ queryKey: qk.wtState() });
    }
  }, [lockedSig, qc]);
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
  const wtState = useQuery(wtStateQuery());
  const archivedSet = useMemo(() => new Set(archive.data ?? []), [archive.data]);
  const stateSlugs = wtState.data?.slugs ?? EMPTY_STATE_SLUGS;
  // Keyed by slug; lets us return the same `WorktreeRow` reference
  // across renders when nothing observable has changed. Without this,
  // every poll-driven refresh produces all-new row identities and
  // forces every downstream `useMemo` / `React.memo` to re-run.
  const rowCache = useRef<Map<string, WorktreeRow>>(new Map());
  const rowsRef = useRef<WorktreeRow[]>([]);

  const worktrees = useMemo(
    () => (wtList.data ?? []).filter((w) => !w.isMain),
    [wtList.data],
  );

  const rowLayout = useMemo(() => {
    // Per-worktree PR lookup — used for the row's `pr` field further
    // down. Hoisted so the GitHub map is only walked once per worktree
    // per actual row-input change.
    const prsByIndex = worktrees.map((wt) =>
      pickPrForWorktree(wt, github.data?.prs),
    );

    // Build the manifest layout index once per row-input change:
    // branch → (layout, node). This is the SOLE source of stack
    // membership, order, and the diff base. A worktree whose branch
    // isn't a manifest slice is flat.
    const stackIndex = buildStackIndex(Object.values(wtState.data?.stacks ?? {}));
    const stackEntryByIndex = worktrees.map((wt) => stackIndex.byBranch.get(wt.branch));

    // Holistic-origin worktrees: the branch a stack was carved from.
    // Held separately from slices in the manifest, so it never appears
    // in `byBranch`. We pin it to the bottom of its stack section
    // (index past the last slice) and tag it `isHolistic` so the list
    // renders it as a dim source node rather than a slice.
    const holisticByBranch = new Map<string, { stackId: string; sliceCount: number }>();
    for (const layout of stackIndex.layouts) {
      const hb = layout.manifest.holisticBranch;
      if (hb) holisticByBranch.set(hb, { stackId: layout.stackId, sliceCount: layout.nodes.length });
    }

    // Resolve `stackedOn` (the diff base) once per row-input change
    // from the manifest entry (or the recorded fork base for non-slice
    // worktrees). Single source of truth — every consumer lands in the
    // same per-(slug, base) cache slot.
    const stackedOnByIndex = worktrees.map((wt, i) =>
      resolveStackedOn(stackEntryByIndex[i], worktrees, stateSlugs[wt.slug]?.baseBranch),
    );
    const bases = stackedOnByIndex.map((s) => s?.diffBase ?? null);
    return {
      prsByIndex,
      stackEntryByIndex,
      holisticByBranch,
      stackedOnByIndex,
      bases,
    };
  }, [worktrees, github.data?.prs, wtState.data?.stacks, stateSlugs]);

  const queries = worktrees.flatMap((wt, i) => [
    wtDirtyQuery(wt),
    wtLockQuery(wt),
    wtDeployQuery(wt),
    wtMergedQuery(wt),
    wtGoneQuery(wt),
    wtSyncQuery(wt, rowLayout.bases[i]!),
    wtClaudeQuery(wt),
    wtGitActivityQuery(wt, rowLayout.bases[i]!),
    wtConflictQuery(wt, rowLayout.bases[i]!),
  ]);

  // `combine` projects each query observer to the exact fields row
  // derivation consumes. TanStack structurally shares this combined
  // array, so unrelated App renders don't force the whole row pipeline
  // to reconstruct.
  const results = useQueries({ queries, combine: combineQuerySnapshots });

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

  // Gated on `aiEnabled`: the diff context exists only to feed
  // `aiSummaryQuery`, so with AI unconfigured there's no consumer — and
  // running it would dispatch a worker-pool job (spawning the pool) per
  // worktree just to compute a hash nothing reads.
  const diffResults = useQueries({
    queries: worktrees.map((wt, i) => ({
      ...wtDiffContextQuery(wt, rowLayout.bases[i]!),
      enabled: aiEnabled && !busyByIndex[i],
    })),
    combine: combineQueryData,
  });

  // Hash-keyed AI summary: a diff change re-keys the query, the
  // observer cache-misses for the new hash, and `keepPreviousData`
  // keeps the prior summary on screen while the new fetch runs. No
  // mismatch effect required — the cache key change *is* the trigger.
  const aiResults = useQueries({
    queries: worktrees.map((wt, i) => {
      const ctx = diffResults[i] ?? null;
      return {
        ...aiSummaryQuery(wt.slug, ctx),
        enabled: aiEnabled && !busyByIndex[i] && !!ctx,
        placeholderData: keepPreviousData,
      };
    }),
    combine: combineQueryData,
  });

  // First-commit subject — non-AI fallback for the title resolution
  // chain. Cheap (one `git log`); paused only while busy so we don't
  // race a destroying worktree's git state.
  const firstCommitResults = useQueries({
    queries: worktrees.map((wt, i) => ({
      ...wtFirstCommitQuery(wt),
      enabled: !busyByIndex[i],
    })),
    combine: combineQueryData,
  });

  const rows = useMemo(() => {
    // Effective order map populated during row construction so the
    // section-aware sorter below can read it without re-walking the
    // stack-section topology.
    const effectiveOrders = new Map<string, number>();

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
        conflict: reuseField(prev?.fields.conflict, toFieldState(fieldArr[8] as FieldState<MergeConflictProbe>)),
      };
      const nextStatus = deriveStatus(wt, fields);
      const status = prev && statusEq(prev.status, nextStatus) ? prev.status : nextStatus;
      const pr = rowLayout.prsByIndex[i];
      const mq = wt.branch ? github.data?.mergeQueue?.[wt.branch] : undefined;
      const stackedOn = rowLayout.stackedOnByIndex[i] ?? null;
      const archived = archivedSet.has(wt.slug);
      // Effective section: a manifest slice's stack section overrides the
      // stored manual section. Archived rows skip the override so the
      // archived bucket stays homogeneous at the bottom of the list.
      const manualSection = stateSlugs[wt.slug]?.section ?? null;
      const entry = archived ? undefined : rowLayout.stackEntryByIndex[i];
      const node = entry?.node;
      const holistic = archived || node ? undefined : rowLayout.holisticByBranch.get(wt.branch);
      const stack: StackRowInfo | null = node
        ? {
            stackId: node.stackId,
            ordinal: node.ordinal,
            pos: node.pos,
            lane: node.lane,
            depth: node.depth,
            index: node.index,
            isHolistic: false,
          }
        : holistic
          ? {
              stackId: holistic.stackId,
              ordinal: 0,
              pos: "single",
              lane: 0,
              depth: 0,
              index: holistic.sliceCount,
              isHolistic: true,
            }
          : null;
      const section = stack ? stackSectionKey(stack.stackId) : manualSection;
      const sectionIsStack = stack !== null;
      effectiveOrders.set(
        wt.slug,
        stack?.index ?? stateSlugs[wt.slug]?.order ?? -Infinity,
      );
      const llmTitle = aiResults[i]?.title ?? null;
      const llmBrief = aiResults[i]?.brief ?? null;
      const prTitle = pr?.title ?? null;
      const commitTitle = firstCommitResults[i] ?? null;
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
        prev.fields.conflict === fields.conflict &&
        prev.status === status &&
        prev.pr === pr &&
        prev.mq === mq &&
        prev.archived === archived &&
        prev.title === title &&
        prev.titleSource === titleSource &&
        prev.brief === llmBrief &&
        prev.section === section &&
        prev.sectionIsStack === sectionIsStack &&
        stackInfoEq(prev.stack, stack) &&
        stackedOnEq(prev.stackedOn, stackedOn)
      ) {
        return prev;
      }
      // Reuse prev's stackedOn / stack references when value-equal so
      // memoized children downstream skip the work.
      const stackedOnOut = prev && stackedOnEq(prev.stackedOn, stackedOn)
        ? prev.stackedOn
        : stackedOn;
      const stackOut = prev && stackInfoEq(prev.stack, stack) ? prev.stack : stack;
      const next: WorktreeRow = {
        wt,
        fields,
        status,
        pr,
        mq,
        stackedOn: stackedOnOut,
        stack: stackOut,
        archived,
        title,
        titleSource,
        brief: llmBrief,
        section,
        sectionIsStack,
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
      effectiveOrders,
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
    return rows;
  }, [
    worktrees,
    results,
    github.data?.mergeQueue,
    rowLayout,
    archivedSet,
    stateSlugs,
    aiResults,
    firstCommitResults,
    wtState.data?.sectionsOrder,
  ]);

  return {
    rows,
    isLoading: wtList.isLoading,
  };
}
