import { useEffect, useMemo, useRef } from "react";
import { keepPreviousData, useQueries, useQuery, useQueryClient } from "@tanstack/react-query";
import type { NotifyOnChangeProps } from "@tanstack/react-query";

import type { ClaudeStatus } from "../../core/harness/claude/jsonl.ts";
import { config } from "../../core/config.ts";
import type { MergeConflictProbe } from "../../core/git.ts";
import type { GitActivity } from "../../core/git-activity.ts";
import { pickPrForWorktree } from "../../core/github.ts";
import { lockAge, lockLabel } from "../../core/locks.ts";
import { latestLogFor } from "../../core/logs.ts";
import {
  edgeIsStaleByTime,
  edgeOrders,
  topoOrderSlugs,
  type MergeEdge,
} from "../../core/merge-edges.ts";
import { buildStackIndex } from "../../core/stack-layout.ts";
import { slugLabel } from "../../core/stage.ts";
import type { LockMeta, MergeQueueEntry, PullRequest, Status, Worktree } from "../../core/types.ts";
import { StatusKind } from "../../core/types.ts";
import {
  LANDED_RANK,
  owesPostMergeVerification,
  workRecordRank,
  workStateRank,
  type WorkStatusRecord,
} from "../../core/work-status.ts";
import type { SyncState } from "../../core/worktree.ts";
import {
  GROUP_INBOX,
  STACK_SECTION_PREFIX,
  type WtState,
} from "../../core/wtstate.ts";
import { type DevServerStatus } from "../../core/dev-server.ts";
import { useGithub, useIssueStatuses } from "../../state/hooks.ts";
import { resolveIssueId } from "../../core/issue-tracker.ts";
import type { GithubData } from "../../state/queries/github.ts";
import { qk } from "../../state/keys.ts";
import {
  aiSummaryQuery,
  archiveQuery,
  tmuxSessionsQuery,
  worktreesQuery,
  wtClaudeQuery,
  wtDeployQuery,
  wtDevQuery,
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
  dev: FieldState<DevServerStatus>;
  merged: FieldState<boolean>;
  gone: FieldState<boolean>;
  sync: FieldState<SyncState>;
  claude: FieldState<ClaudeStatus>;
  gitActivity: FieldState<GitActivity>;
  conflict: FieldState<MergeConflictProbe>;
};

/**
 * Stack relationship for a worktree, with the resolved diff base. One
 * explicit source, no inference beyond it (no reflog / PR-base
 * guessing): the per-slug `baseBranch` recorded by `wt new --base` /
 * `wt base` / restack reconciles.
 *
 * `slug` is `null` when the parent branch isn't materialized as a live
 * worktree; the consumer can still use the diff base for diffing but
 * has no row to draw a UI hint to.
 */
export type StackedOn = {
  slug: string | null;
  branch: string;
  /** Ref to use for `git diff <diffBase>...HEAD`. */
  diffBase: string;
};

/**
 * Placement of a worktree within its inferred stack (worktrees chained
 * by their recorded fork bases). Drives the list's tree spine +
 * `null` for any worktree that isn't part of a stack.
 */
export type StackRowInfo = {
  /** Stack identity: the root member's branch. */
  stackId: string;
  /** Parallel-lane index → connector color (0 = main spine, dim). */
  lane: number;
  /** Depth from the stack root (root = 0). */
  depth: number;
  /** Display index within the stack (spine order). */
  index: number;
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
   * Resolved stack parent (from the recorded fork base). `null` for
   * trunk-targeted worktrees. Drives the diff base for
   * `wtDiffContextQuery` (so the AI summary describes only what this
   * worktree adds on top of its parent); the relationship is shown by
   * the tree spine in the list (see `row.stack`), not a separate badge.
   */
  stackedOn: StackedOn | null;
  /**
   * Placement within an inferred stack, or `null` when this worktree
   * isn't part of one. When set, `section` is the stack's synthetic key
   * and `sectionIsStack` is true.
   */
  stack: StackRowInfo | null;
  /**
   * Secondary GitHub issue number from the slug-state record (`wt new
   * --gh` / `wt issue --gh`). The primary id uses its stored override
   * or the slug; `i` / `y i` fall back here when it has no URL.
   */
  githubIssue: number | null;
  /** Stored tracker-id override; null = fall back to parsing the slug. */
  issueId: string | null;
  /** External tracker truth, separate from the agent's asserted work status. */
  issueStatus?: FieldState<string> & { optimistic: boolean };
  /** Successful wt new creation event, absent on pre-existing checkouts. */
  createdAt?: string;
  /**
   * Agent-asserted work status (`wt status` / the `u` picker), straight
   * from wtstate. `null` = never asserted. The list dot renders the
   * EFFECTIVE state (this plus the session-asking override) via
   * `workStatusBadge`; the section-internal auto-sort ranks on this.
   */
  work: WorkStatusRecord | null;
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
   * Effective section. A stack member's section is the synthetic stack
   * key (`STACK_SECTION_PREFIX + stackId`), which overrides any manual
   * placement; otherwise it's the slug's stored `slugs[slug].section`.
   * `null` means the unsectioned inbox.
   */
  section: string | null;
};

const FIELD_ORDER = [
  "dirty",
  "lock",
  "deploy",
  "dev",
  "merged",
  "gone",
  "sync",
  "claude",
  "gitActivity",
  "conflict",
] as const;

/** Index of the lock field in the flat `useQueries` result array. */
const LOCK_FIELD_INDEX = FIELD_ORDER.indexOf("lock");

const EMPTY_STATE_SLUGS: WtState["slugs"] = {};

export type WorktreeRowsResult = {
  rows: WorktreeRow[];
  githubData: GithubData | undefined;
  archivedKeys: ReadonlySet<string>;
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

/**
 * The exact result props `combineQuerySnapshots` / `combineQueryData`
 * read. Declaring them turns OFF TanStack's tracked-props proxy for
 * these batches — see the comment at the `useQueries` call.
 */
const SNAPSHOT_PROPS: NotifyOnChangeProps = [
  "data",
  "isStale",
  "isFetching",
  "isLoading",
  "error",
];

const DATA_PROPS: NotifyOnChangeProps = ["data"];

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
export {
  GROUP_ARCHIVED,
  GROUP_INBOX,
  STACK_SECTION_PREFIX,
  stackSectionKey,
} from "../../core/wtstate.ts";

function stackedOnEq(a: StackedOn | null, b: StackedOn | null): boolean {
  if (a === b) return true;
  if (!a || !b) return false;
  return (
    a.slug === b.slug &&
    a.branch === b.branch &&
    a.diffBase === b.diffBase
  );
}

function stackInfoEq(a: StackRowInfo | null, b: StackRowInfo | null): boolean {
  if (a === b) return true;
  if (!a || !b) return false;
  return (
    a.stackId === b.stackId &&
    a.lane === b.lane &&
    a.depth === b.depth &&
    a.index === b.index
  );
}

/**
 * Resolve `stackedOn` (the diff base) for a worktree from its recorded
 * fork base (`wt new --base` → slug-state `baseBranch`). A trunk-based
 * (or record-free) worktree renders flat (null), as does a nonsense
 * self-referential record (same guard `buildStackIndex` applies — the
 * two paths must agree or the row would diff against itself while the
 * tree shows it flat). The parent's live worktree slug (when it exists)
 * lets the list draw the relationship.
 */
function resolveStackedOn(
  ownBranch: string,
  worktrees: readonly Worktree[],
  forkBase: string | undefined,
): StackedOn | null {
  if (!forkBase || forkBase === config.branch.base) return null;
  if (forkBase === ownBranch) return null; // self-loop guard
  const parentWt = worktrees.find((w) => w.branch === forkBase);
  return {
    slug: parentWt?.slug ?? null,
    branch: forkBase,
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
 * Urgency rank for the status-first sort inside a (non-stack) section.
 * Merged/gone rows sink last whatever they asserted (landed is landed);
 * everything else ranks on the asserted work status — deliberately NOT
 * the effective state (session-asking upgrades tint the dot but don't
 * reorder rows; sorting on a transient signal would make the list
 * twitch every time an agent pauses on a prompt).
 */
export function rowWorkRank(row: WorktreeRow): number {
  if (row.status.kind === StatusKind.Merged || row.status.kind === StatusKind.Gone) {
    // Except when the landing is what MAKES it actionable. A branch
    // carrying `--verify-after-merge` owes a check that could not run
    // until it deployed, so sinking it to the bottom on the day it
    // becomes runnable is the exact failure the field exists to
    // prevent — the row reads as covered, the sweep takes the
    // checkout, and nobody ever finds out the check never happened.
    // It ranks as what it now is: needs-testing.
    if (owesPostMergeVerification(row.work, true)) {
      return workStateRank("needs-testing");
    }
    return LANDED_RANK;
  }
  return workRecordRank(row.work);
}

/**
 * Section-aware sort for the active (non-archived) rows. Display order
 * is one unified ranked list of GROUPS — stack sections, the
 * unsectioned inbox (`GROUP_INBOX` sentinel), and manual named
 * sections — exactly as they appear in `sectionsOrder` (readWtState
 * self-heals it: dead manual groups drop, and a pre-unification file is
 * seeded with the legacy inbox/manual layout). Within each group, rows
 * sort status-first when `[ui] sort = "status"` (the default): work-
 * status urgency rank, then the manual `state.order` as the stable
 * tie-break — so same-status rows keep their hand order, statusless
 * rows keep exactly the old behavior, and `J`/`K` still reorders within
 * a rank. Stack sections are exempt (spine order IS the layout), as is
 * `sort = "manual"`. Unstated orders float to the top (-Infinity) so
 * brand-new worktrees land at the top of their rank band. Groups not
 * yet ranked (a freshly created stack mid-render, post-rename quirk)
 * degrade predictably: stack keys sort to the front (where new stacks
 * live), manual names to the end, then alphabetically — display stays
 * stable until the self-heal catches up on the next read.
 *
 * Returned as a fresh array; the caller is responsible for combining
 * with the archived rows and any rows-array identity stabilization.
 * Cursor stability across re-sorts is structural: the selection is
 * keyed by slug (see app.tsx `sel`), never by index.
 */
function sortActiveRows(
  active: WorktreeRow[],
  unsortedIndex: ReadonlyMap<string, number>,
  effectiveOrders: ReadonlyMap<string, number>,
  sectionsOrder: readonly string[],
  statusSort: boolean,
): WorktreeRow[] {
  const rank = new Map<string, number>();
  for (let i = 0; i < sectionsOrder.length; i++) {
    rank.set(sectionsOrder[i]!, i);
  }
  const groupOf = (r: WorktreeRow): string => r.section ?? GROUP_INBOX;
  // A stack sorts as ONE contiguous unit inside its section, taking the
  // position its most urgent member would take. Members have genuinely
  // different statuses (finished parent, unstarted child), so ranking
  // them independently would interleave unrelated rows through a spine
  // and leave a connector pointing at a parent several rows away.
  // Members that live in DIFFERENT sections are separate units — a
  // split stack is legitimate, and each half sorts where it sits.
  const unitKey = (r: WorktreeRow): string =>
    r.stack ? `${groupOf(r)}\u0000${r.stack.stackId}` : `\u0000${r.wt.slug}`;
  const unitRank = new Map<string, number>();
  const unitOrder = new Map<string, number>();
  const unitDepth = new Map<string, number>();
  for (const r of active) {
    const k = unitKey(r);
    const rank = rowWorkRank(r);
    const prevRank = unitRank.get(k);
    if (prevRank === undefined || rank < prevRank) unitRank.set(k, rank);
    // The unit's slot is the ROOT's manual order (shallowest member
    // present), so `J`/`K` on the root still moves the whole block.
    const depth = r.stack?.depth ?? 0;
    const prevDepth = unitDepth.get(k);
    if (prevDepth === undefined || depth < prevDepth) {
      unitDepth.set(k, depth);
      unitOrder.set(k, effectiveOrders.get(r.wt.slug) ?? -Infinity);
    }
  }
  // Unranked stack keys sort to the front (where new stacks live) —
  // but never above an Inbox that currently holds the top slot: the
  // Inbox is where new worktrees land, and a freshly formed stack
  // displacing it from the top reads as the list rearranging itself.
  // When the user has deliberately ranked something above the Inbox,
  // front means front.
  const unrankedStackRank = sectionsOrder[0] === GROUP_INBOX ? 0.5 : -1;
  const rankOf = (g: string): number =>
    rank.get(g) ??
    (g.startsWith(STACK_SECTION_PREFIX) ? unrankedStackRank : Number.MAX_SAFE_INTEGER);
  return active.slice().sort((a, b) => {
    const groupA = groupOf(a);
    const groupB = groupOf(b);
    if (groupA !== groupB) {
      const rankA = rankOf(groupA);
      const rankB = rankOf(groupB);
      if (rankA !== rankB) return rankA - rankB;
      return groupA.localeCompare(groupB);
    }
    const keyA = unitKey(a);
    const keyB = unitKey(b);
    if (keyA === keyB) {
      // Same stack: spine order IS the layout (base-record chain).
      const ia = a.stack?.index ?? 0;
      const ib = b.stack?.index ?? 0;
      if (ia !== ib) return ia - ib;
      return (unsortedIndex.get(a.wt.slug) ?? 0) - (unsortedIndex.get(b.wt.slug) ?? 0);
    }
    if (statusSort) {
      const wr = (unitRank.get(keyA) ?? 0) - (unitRank.get(keyB) ?? 0);
      if (wr !== 0) return wr;
    }
    const orderA = unitOrder.get(keyA) ?? -Infinity;
    const orderB = unitOrder.get(keyB) ?? -Infinity;
    if (orderA !== orderB) return orderA - orderB;
    return (unsortedIndex.get(a.wt.slug) ?? 0) - (unsortedIndex.get(b.wt.slug) ?? 0);
  });
}

/**
 * Post-sort ordering pass for merge edges (`wt edge`): within each
 * non-stack section bucket, permute rows so every FRESH ordering edge
 * (before/enables, endpoints both in the bucket) puts `from` above
 * `to` — rendering order becomes merge order, composing with the
 * human's sections (they own which batch, edges own order within it).
 * Stable: rows without applicable edges keep their sorted positions,
 * and bucket slots themselves never move. Freshness is commit-time
 * decay (`edgeIsStaleByTime`, the same signal family as the
 * work-status stale dot): once either endpoint commits past the
 * assert, the edge quietly stops steering — decay, not diligence.
 * Stack sections are exempt (spine order IS the layout).
 */
function applyMergeEdgeOrder(
  rows: WorktreeRow[],
  edges: readonly MergeEdge[],
): WorktreeRow[] {
  if (edges.length === 0) return rows;
  const bySlug = new Map(rows.map((r) => [r.wt.slug, r]));
  const lastCommitOf = (slug: string): number | null | undefined =>
    bySlug.get(slug)?.fields.gitActivity.data?.lastCommitMs;
  const fresh = edges.filter(
    (e) => edgeOrders(e.kind) && !edgeIsStaleByTime(e, lastCommitOf),
  );
  if (fresh.length === 0) return rows;
  // Bucket rows by section, tracking each bucket's index slots.
  const buckets = new Map<string, number[]>();
  for (let i = 0; i < rows.length; i++) {
    const r = rows[i]!;
    if (r.stack) continue;
    const key = r.section ?? GROUP_INBOX;
    const list = buckets.get(key);
    if (list) list.push(i);
    else buckets.set(key, [i]);
  }
  let out: WorktreeRow[] | null = null;
  for (const slots of buckets.values()) {
    if (slots.length < 2) continue;
    const slugs = slots.map((i) => rows[i]!.wt.slug);
    const ordered = topoOrderSlugs(slugs, fresh);
    if (ordered.every((s, i) => s === slugs[i])) continue;
    out ??= [...rows];
    for (let i = 0; i < slots.length; i++) {
      out[slots[i]!] = bySlug.get(ordered[i]!)!;
    }
  }
  return out ?? rows;
}

/**
 * Watch the set of slugs that currently hold a lock. When a slug
 * transitions from held → released, invalidate the worktree list so
 * a destroyed slug drops promptly instead of lingering as a stale
 * merged/gone candidate until its 15s staleTime expires — and refresh
 * the released slug's own field queries. Everything a lock-holding op
 * touches (upstream set after `git worktree add`, install churn, sst
 * writes, branch deletes) was fetched mid-op by whichever field
 * queries mounted while the row was busy; without this wave those
 * fields keep the mid-op answer until something else happens to
 * invalidate `["wt"]`. The per-slug prefix also covers `diffContext`,
 * so a fresh create's AI title kicks off right when setup finishes.
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
    const released: string[] = [];
    for (const slug of prev) {
      if (!curr.has(slug)) released.push(slug);
    }
    prevLockedRef.current = curr;
    if (released.length > 0) {
      for (const slug of released) {
        void qc.invalidateQueries({ queryKey: qk.wt(slug).all() });
      }
      void qc.invalidateQueries({ queryKey: qk.worktrees() });
      // Destroys (and `init`) mutate state.json from the child process
      // — refresh the wtState query so the row aggregator sees those
      // mutations (slug-state reap, fork-base reparents) without
      // waiting for the staleTime to expire.
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
  const issues = useIssueStatuses();
  const archive = useQuery(archiveQuery());
  const wtState = useQuery(wtStateQuery());
  // Batched tmux session set — read here (rather than letting
  // `wtDevQuery` spawn its own per-worktree `has-session`) so the dev
  // badge rides this query's freshness (5s poll + push invalidation)
  // instead of a redundant per-row shell-out. `undefined` data (not
  // yet loaded) is distinguished from "loaded, empty" via `devSlugs`
  // being `null` so `wtDevQuery` falls back to its own tmux check.
  const tmuxSessions = useQuery(tmuxSessionsQuery());
  const devSlugs = useMemo(
    () => (tmuxSessions.data ? new Set(tmuxSessions.data.dev) : null),
    [tmuxSessions.data],
  );
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

    // Build the inferred-stack index once per row-input change:
    // branch → (layout, node), derived from the live worktrees + their
    // recorded fork bases. This is the SOLE source of stack membership
    // and order. A worktree whose base chain doesn't reach another live
    // worktree is flat.
    const stackIndex = buildStackIndex(
      worktrees.map((wt) => ({
        slug: wt.slug,
        branch: wt.branch,
        baseBranch: stateSlugs[wt.slug]?.baseBranch,
      })),
    );
    const stackEntryByIndex = worktrees.map((wt) => stackIndex.byBranch.get(wt.branch));

    // Resolve `stackedOn` (the diff base) once per row-input change from
    // the recorded fork base. Single source of truth — every consumer
    // lands in the same per-(slug, base) cache slot.
    const stackedOnByIndex = worktrees.map((wt) =>
      resolveStackedOn(wt.branch, worktrees, stateSlugs[wt.slug]?.baseBranch),
    );
    const bases = stackedOnByIndex.map((s) => s?.diffBase ?? null);
    return {
      prsByIndex,
      stackEntryByIndex,
      stackedOnByIndex,
      bases,
    };
  }, [worktrees, github.data?.prs, stateSlugs]);

  const queries = worktrees.flatMap((wt, i) => [
    wtDirtyQuery(wt),
    wtLockQuery(wt),
    wtDeployQuery(wt),
    wtDevQuery(wt, devSlugs ? devSlugs.has(wt.slug) : null),
    wtMergedQuery(wt),
    wtGoneQuery(wt),
    wtSyncQuery(wt, rowLayout.bases[i]!),
    wtClaudeQuery(wt),
    wtGitActivityQuery(wt, rowLayout.bases[i]!),
    wtConflictQuery(wt, rowLayout.bases[i]!),
  ].map((q) => ({ ...q, notifyOnChangeProps: SNAPSHOT_PROPS })));

  // `combine` projects each query observer to the exact fields row
  // derivation consumes. TanStack structurally shares this combined
  // array, so unrelated App renders don't force the whole row pipeline
  // to reconstruct.
  const results = useQueries({ queries, combine: combineQuerySnapshots });

  // Diff context + AI summary observers for every worktree, so the list
  // panel can render LLM-generated titles next to each row. The cache is
  // content-addressed and persisted, so steady-state these are no-op
  // hits; only new/changed worktrees start a naming harness. Gated on
  // the lock state from the batch above so we don't race a destroying
  // worktree's git state.
  const aiEnabled = !!config.naming;
  const busyByIndex = worktrees.map((_, i) => {
    const lock = results[i * FIELD_ORDER.length + LOCK_FIELD_INDEX]?.data as
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
      notifyOnChangeProps: DATA_PROPS,
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
        notifyOnChangeProps: DATA_PROPS,
      };
    }),
    combine: combineQueryData,
  });

  // First-commit subject — non-AI fallback for the title resolution
  // chain. Cheap (one `git log`); paused only while busy so we don't
  // race a destroying worktree's git state.
  const firstCommitResults = useQueries({
    queries: worktrees.map((wt, i) => ({
      ...wtFirstCommitQuery(wt, stateSlugs[wt.slug]?.baseBranch ?? null),
      enabled: !busyByIndex[i],
      notifyOnChangeProps: DATA_PROPS,
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
        dev: reuseField(prev?.fields.dev, toFieldState(fieldArr[3] as FieldState<DevServerStatus>)),
        merged: reuseField(prev?.fields.merged, toFieldState(fieldArr[4] as FieldState<boolean>)),
        gone: reuseField(prev?.fields.gone, toFieldState(fieldArr[5] as FieldState<boolean>)),
        sync: reuseField(prev?.fields.sync, toFieldState(fieldArr[6] as FieldState<SyncState>)),
        claude: reuseField(prev?.fields.claude, toFieldState(fieldArr[7] as FieldState<ClaudeStatus>)),
        gitActivity: reuseField(prev?.fields.gitActivity, toFieldState(fieldArr[8] as FieldState<GitActivity>)),
        conflict: reuseField(prev?.fields.conflict, toFieldState(fieldArr[9] as FieldState<MergeConflictProbe>)),
      };
      const nextStatus = deriveStatus(wt, fields);
      const status = prev && statusEq(prev.status, nextStatus) ? prev.status : nextStatus;
      const pr = rowLayout.prsByIndex[i];
      const mq = wt.branch ? github.data?.mergeQueue?.[wt.branch] : undefined;
      const stackedOn = rowLayout.stackedOnByIndex[i] ?? null;
      const archived = archivedSet.has(wt.slug);
      // Effective section: a stack member's stack section overrides the
      // stored manual section. Archived rows skip the override so the
      // archived bucket stays homogeneous at the bottom of the list.
      const manualSection = stateSlugs[wt.slug]?.section ?? null;
      const entry = archived ? undefined : rowLayout.stackEntryByIndex[i];
      const node = entry?.node;
      const stack: StackRowInfo | null = node
        ? {
            stackId: node.stackId,
            lane: node.lane,
            depth: node.depth,
            index: node.index,
          }
        : null;
      // Sections own the vertical axis. A stack is a RELATIONSHIP
      // between rows, drawn as a spine inside whichever section each
      // member was placed in — it is not a place. The derived grouping
      // used to override the stated one, which evicted the human's own
      // section name (three stacks rendered as three headers all
      // literally named "stack") and made stacked worktrees unfilable.
      const section = manualSection;
      effectiveOrders.set(wt.slug, stateSlugs[wt.slug]?.order ?? -Infinity);
      const githubIssue = stateSlugs[wt.slug]?.githubIssue ?? null;
      const issueId = stateSlugs[wt.slug]?.issueId ?? null;
      const resolvedId = resolveIssueId(wt.slug, issueId);
      const optimistic = !!resolvedId && issues.expected.has(resolvedId);
      const nextIssueStatus = config.issueTracker?.statusCommand && resolvedId
        ? { ...toFieldState(issues), data: issues.data?.[resolvedId], optimistic }
        : undefined;
      const issueStatus = nextIssueStatus && prev?.issueStatus?.optimistic === optimistic
        ? reuseField(prev.issueStatus, nextIssueStatus) as typeof nextIssueStatus
        : nextIssueStatus;
      const createdAt = stateSlugs[wt.slug]?.createdAt;
      const work = stateSlugs[wt.slug]?.work ?? null;
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
        prev.fields.dev === fields.dev &&
        prev.fields.merged === fields.merged &&
        prev.fields.gone === fields.gone &&
        prev.fields.sync === fields.sync &&
        prev.fields.claude === fields.claude &&
        prev.fields.gitActivity === fields.gitActivity &&
        prev.fields.conflict === fields.conflict &&
        prev.status === status &&
        prev.pr === pr &&
        prev.mq === mq &&
        prev.githubIssue === githubIssue &&
        prev.issueId === issueId &&
        prev.issueStatus === issueStatus &&
        prev.createdAt === createdAt &&
        prev.work === work &&
        prev.archived === archived &&
        prev.title === title &&
        prev.titleSource === titleSource &&
        prev.brief === llmBrief &&
        prev.section === section &&
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
        githubIssue,
        issueId,
        issueStatus,
        createdAt,
        work,
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
    const active = applyMergeEdgeOrder(
      sortActiveRows(
        unsorted.filter((r) => !r.archived),
        listIndexOf,
        effectiveOrders,
        sectionsOrder,
        config.ui.sort === "status",
      ),
      wtState.data?.edges ?? [],
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
    issues.data,
    issues.expected,
    issues.error,
    issues.isFetching,
    issues.isLoading,
    issues.isStale,
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
    /** Shared PR/CI snapshot, including local and remote fleet branches. */
    githubData: github.data,
    /** Location-aware keys from the local fleet archive ledger. */
    archivedKeys: archivedSet,
    isLoading: wtList.isLoading,
  };
}
