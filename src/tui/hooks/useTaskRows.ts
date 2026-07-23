/**
 * Task-inbox row derivation for hub mode. Turns the worktree list (from
 * `useWorktreeRows`), the review-request PR list, and the live-session /
 * automation-adjacent signals every other panel already computes into a
 * single flat, sorted `TaskItem[]` — one line-item per "thing that might
 * need you", already bucketed and ordered by urgency.
 *
 * PURE derivation glued to real sources. `core/task-state.ts` owns the
 * bucket precedence table and the sort comparator (it's fully unit-
 * tested there); this hook's only job is building the `TaskSignals` that
 * feed it from the row pipeline's actual field data, and folding stacks
 * into a single focus-slice item (or splicing them open when expanded).
 * No fetching happens here beyond `useSyncExternalStore` on
 * `taskFocusStore` — everything else arrives via `rows` / `reviewRequests`
 * / `wtState`, already fetched by the caller's existing hooks.
 */
import { useMemo, useRef, useSyncExternalStore } from "react";

import type { SessionTail } from "../../core/harness/claude/jsonl.ts";
import type { DerivedState } from "../../core/harness/status.ts";
import { isCleanCandidate } from "../app-helpers.ts";
import type { ReviewRequestPr } from "../../core/github.ts";
import {
  compareTasks,
  deriveTaskState,
  TASK_BUCKET_ORDER,
  type TaskBucket,
  type TaskManual,
  type TaskPrSignals,
  type TaskSignals,
  type TaskState,
} from "../../core/task-state.ts";
import { taskFocusStore } from "../../core/task-focus.ts";
import {
  StatusKind,
  type MergeQueueEntry,
  type PrChecks,
  type PrReview,
  type PullRequest,
} from "../../core/types.ts";
import type { WtState } from "../../core/wtstate.ts";
import {
  DOTFILES_SLOT,
  MAIN_CLONE_SLOT,
  WT_SOURCE_SLOT,
  type SessionSlot,
} from "../sessions/slots.ts";
import type { ActiveSessionGlyph } from "./useHarnessSessions.ts";
import type { WorktreeRow } from "./useWorktreeRows.ts";

/**
 * Section-grouping key for a `TaskItem` — the two synthetic overlay
 * buckets ("pinned"/"snoozed", which outrank the derived bucket for
 * display per `displayBucketFor`) and the Sessions-group sentinel,
 * union'd with the real `TaskBucket`s. Typed as its own union (rather
 * than `string`) so consumers can switch on it without an `as
 * TaskBucket` cast — narrowing the three literals out of a `string`
 * left the residual as an untyped string, whereas narrowing them out
 * of this union leaves exactly `TaskBucket`.
 */
export type DisplayBucket = "pinned" | "snoozed" | "sessions" | TaskBucket;

/**
 * One line-item in the task inbox. Three kinds share the same
 * bucket/manual/detail/displayBucket shape so the panel can render and
 * sort them uniformly:
 *
 *  - `wt` — a standalone (non-stacked) worktree, or one member of a
 *    stack the user has expanded (`inExpandedStack: true`).
 *  - `stack` — a collapsed stack, folded to its highest-priority
 *    member's ("focus slice") state. `row` is that member, purely so
 *    the panel can reuse `BadgeCluster`/`rowLabel` machinery that
 *    expects a `WorktreeRow`; `members` carries the whole spine for
 *    reference (e.g. a future "expand" affordance keyed off it).
 *  - `pr` — a review-request PR with no local worktree at all.
 */
export type TaskItem =
  | {
      kind: "wt";
      key: string /* slug */;
      row: WorktreeRow;
      state: TaskState;
      manual: TaskManual;
      detail: string | null;
      displayBucket: DisplayBucket;
      inExpandedStack: boolean;
    }
  | {
      kind: "stack";
      key: string /* stack section key */;
      row: WorktreeRow /* focus slice */;
      state: TaskState;
      manual: TaskManual;
      detail: string | null;
      displayBucket: DisplayBucket;
      label: string;
      members: readonly WorktreeRow[];
    }
  | {
      kind: "pr";
      key: string /* pr.url */;
      pr: ReviewRequestPr;
      state: TaskState;
      detail: string | null;
      displayBucket: DisplayBucket;
    }
  | {
      /**
       * A special (non-worktree) harness session slot — the main
       * clone, the wt source repo, the dotfiles. Pinned as a group at
       * the BOTTOM of the inbox under its own "Sessions" divider:
       * collapsed to just the main-clone entry, Tab-expanded to all
       * three. Selecting one live-follows its session like any task;
       * there are no dedicated keybindings.
       */
      kind: "slot";
      key: string /* slot:<slug> */;
      slot: SessionSlot;
      displayBucket: "sessions";
      /** True when the group is collapsed (the visible main entry hints at Tab). */
      collapsedGroup: boolean;
    };

export type TaskRowsResult = { tasks: readonly TaskItem[]; isLoading: boolean };

/**
 * Session tail with the greatest `lastEntryMs` across a worktree's
 * claude sessions (primary + named). This is the row's single "most
 * recently active conversation" — `turnEndedAt`, `lastActivityMs`, and
 * `detail` (pendingAsk / lastAssistantText) all read off it rather than
 * re-scanning the session list per signal.
 */
export function freshestTail(sessions: readonly SessionTail[] | undefined): SessionTail | null {
  let best: SessionTail | null = null;
  let bestMs = -Infinity;
  for (const s of sessions ?? []) {
    if (s.lastEntryMs === null) continue;
    if (s.lastEntryMs > bestMs) {
      best = s;
      bestMs = s.lastEntryMs;
    }
  }
  return best;
}

/** `PrChecks` (lowercase, worktree-row PR shape) → `TaskPrSignals["checks"]`. */
export function prChecksToTaskChecks(c: PrChecks): TaskPrSignals["checks"] {
  switch (c) {
    case "pass":
      return "passing";
    case "fail":
      return "failing";
    case "pending":
      return "pending";
    case "none":
      return "none";
  }
}

/**
 * `PrReview` (the aggregate human-review state on `WorktreeRow.pr`) →
 * `TaskPrSignals["reviewDecision"]`'s GitHub-shaped enum. `pending`
 * (requested, nobody's responded) maps to `REVIEW_REQUIRED` — the
 * closest GitHub concept — and `unrequested`/`none` both collapse to
 * `null` (no decision to report).
 */
export function mapReviewDecision(r: PrReview): TaskPrSignals["reviewDecision"] {
  switch (r) {
    case "approved":
      return "APPROVED";
    case "changes_requested":
      return "CHANGES_REQUESTED";
    case "pending":
      return "REVIEW_REQUIRED";
    case "unrequested":
      return null;
    case "none":
      return null;
  }
}

/**
 * Null when the row has no PR at all — `deriveTaskState`'s `isOpenPr`
 * guard handles the rest — OR when `githubFresh` is false. Persisted
 * PR data restored from the query cache at boot must never drive a
 * bucket decision before the github query has live-fetched this
 * session; `automation-rules.ts`'s freshness guard (see its module doc
 * comment) enforces the exact same hard rule for automation triggers,
 * for the exact same reason: a stale red badge shouldn't be able to
 * fire "needs-you: checks failing" off yesterday's cache.
 */
export function buildPrSignals(
  pr: PullRequest | undefined,
  mq: MergeQueueEntry | undefined,
  githubFresh: boolean,
): TaskPrSignals | null {
  if (!pr || !githubFresh) return null;
  return {
    state: pr.state,
    isDraft: pr.isDraft,
    checks: prChecksToTaskChecks(pr.checks),
    reviewDecision: mapReviewDecision(pr.review),
    autoMergeArmed: pr.autoMerge !== null,
    inMergeQueue: mq != null,
  };
}

export function isTaskBucket(x: string): x is TaskBucket {
  return (TASK_BUCKET_ORDER as readonly string[]).includes(x);
}

/**
 * `TaskManual` from the persisted per-slug wtstate record. A stale
 * `taskSnoozedBucket` (a bucket name that no longer exists — shouldn't
 * happen, but the field is a free-form string in the state file) reads
 * as "not snoozed" rather than throwing; `deriveTaskState` already
 * treats a bucket mismatch as an expired snooze, this just guards the
 * type at the boundary.
 */
function manualFor(wtState: WtState | undefined, slug: string): TaskManual {
  const stored = wtState?.slugs[slug];
  const snoozed = stored?.taskSnoozedBucket;
  return {
    pinned: stored?.taskPinned === true,
    snoozedBucket: snoozed && isTaskBucket(snoozed) ? snoozed : null,
  };
}

/**
 * The row's second-line detail text: the agent's pending question
 * (needs-you/asking) or its last written line (review-output). Every
 * other bucket has nothing more specific to say than `state.reason`
 * already does.
 */
function detailFor(state: TaskState, tail: SessionTail | null): string | null {
  if (state.bucket === "needs-you" && state.reason === "agent is asking") {
    return tail?.pendingAsk ?? null;
  }
  if (state.bucket === "review-output") {
    return tail?.lastAssistantText ?? null;
  }
  return null;
}

/** `"pinned"` / `"snoozed"` outrank the derived bucket for section grouping — same task, different shelf. */
function displayBucketFor(state: TaskState, manual: TaskManual): DisplayBucket {
  if (manual.pinned) return "pinned";
  if (state.snoozed) return "snoozed";
  return state.bucket;
}

type RowTaskData = {
  state: TaskState;
  manual: TaskManual;
  detail: string | null;
  lastActivityMs: number;
};

/**
 * `turnEndedAt` gate: only non-null when the freshest tail's own
 * terminal-ness agrees with the live session state — a session that's
 * since resumed working past an old tail shouldn't still read as "turn
 * ended" just because the jsonl hasn't caught up.
 *
 * Only `"end_turn"` counts as a genuinely COMPLETED turn. `"paused"` is
 * mid-turn — a tool-permission stop, not a finished response — per
 * `core/harness/status.ts`'s own `midTurn` set (which lists
 * `"paused"` alongside `"tool_use"`/`"tool_result"`). A session
 * abandoned mid-pause must fall through to idle/"abandoned" in
 * `deriveTaskState`, not read as unreviewed review-output.
 */
export function computeTurnEndedAt(
  tail: SessionTail | null,
  sessionState: DerivedState | null,
): number | null {
  if (tail === null || tail.lastEntryMs === null) return null;
  if (tail.lastEntryKind !== "end_turn") return null;
  if (sessionState === "working" || sessionState === "polling" || sessionState === "asking") {
    return null;
  }
  return tail.lastEntryMs;
}

/**
 * The full `TaskSignals` → `TaskState` pipeline for one worktree row.
 * Shared by standalone rows and stack members alike — a stack's "focus
 * slice" selection and each expanded member's own item both need this
 * same per-row derivation, so it's computed once per row up front
 * rather than re-derived per consumer.
 */
function computeRowTask(
  row: WorktreeRow,
  activeSessionBySlug: ReadonlyMap<string, ActiveSessionGlyph>,
  activeActions: ReadonlySet<string>,
  wtState: WtState | undefined,
  focusSnapshot: ReadonlyMap<string, number>,
  githubFresh: boolean,
): RowTaskData {
  const slug = row.wt.slug;
  const tail = freshestTail(row.fields.claude.data?.sessions);
  const sessionState = activeSessionBySlug.get(slug)?.state ?? null;
  const turnEndedAt = computeTurnEndedAt(tail, sessionState);
  const probe = row.fields.conflict.data;
  const sig: TaskSignals = {
    sessionState,
    turnEndedAt,
    lastFocusedAt: focusSnapshot.get(slug) ?? null,
    busyLock: row.status.kind === StatusKind.Busy,
    actionRunning: activeActions.has(slug),
    conflict: probe?.status === "conflict",
    midRebase: probe?.status === "rebasing",
    dirty: row.status.kind === StatusKind.Dirty,
    mergedOrGone: isCleanCandidate(row),
    pr: buildPrSignals(row.pr, row.mq, githubFresh),
    lastActivityMs: tail?.lastEntryMs ?? 0,
  };
  const manual = manualFor(wtState, slug);
  const state = deriveTaskState(sig, manual);
  const detail = detailFor(state, tail);
  return { state, manual, detail, lastActivityMs: sig.lastActivityMs };
}

/**
 * One sortable inbox unit — a standalone worktree, a whole stack
 * (collapsed OR expanded), or a review-request PR — carrying the sort
 * key `compareTasks` ranks it by and a `build()` that expands it into
 * its final `TaskItem`(s). A collapsed stack and a standalone worktree
 * both build exactly one item; an expanded stack builds one item per
 * member but still sorts as a single unit keyed on its focus slice, so
 * the whole spine moves together and stays contiguous in the output.
 */
type SortUnit = {
  sortKey: { state: TaskState; manual: TaskManual; lastActivityMs: number };
  build: () => TaskItem[];
};

/**
 * Pick a stack's "focus slice" — the member whose bucket ranks highest
 * (lowest `TASK_BUCKET_ORDER` index = most urgent); ties broken by
 * spine ordinal so the pick is deterministic. Uses the RAW bucket
 * rank, not the snooze-adjusted sort rank — a snoozed member "winning"
 * the focus slice would hide a genuinely urgent stack behind its own
 * snooze. Generic over the member shape (rather than `WorktreeRow`
 * directly) so it's testable with plain data.
 */
export function pickFocusMember<T>(
  members: readonly T[],
  bucketOf: (m: T) => TaskBucket,
  ordinalOf: (m: T) => number,
): T {
  let focus = members[0]!;
  let focusRank = TASK_BUCKET_ORDER.indexOf(bucketOf(focus));
  for (let i = 1; i < members.length; i++) {
    const m = members[i]!;
    const rank = TASK_BUCKET_ORDER.indexOf(bucketOf(m));
    if (rank < focusRank || (rank === focusRank && ordinalOf(m) < ordinalOf(focus))) {
      focus = m;
      focusRank = rank;
    }
  }
  return focus;
}

/** Value-equality for the tiny `TaskState` record — used by `taskItemEq` so a fresh `deriveTaskState()` result with identical fields doesn't defeat item reuse. */
function taskStateEq(a: TaskState, b: TaskState): boolean {
  return a.bucket === b.bucket && a.reason === b.reason && a.snoozed === b.snoozed;
}

/** Value-equality for `TaskManual`, same rationale as `taskStateEq`. */
function taskManualEq(a: TaskManual, b: TaskManual): boolean {
  return a.pinned === b.pinned && a.snoozedBucket === b.snoozedBucket;
}

/** Element-wise identity equality — `stack` items carry their member list by reference per-slot, not the array's own identity (which is rebuilt every pass). */
function membersEq(a: readonly WorktreeRow[], b: readonly WorktreeRow[]): boolean {
  if (a.length !== b.length) return false;
  for (let i = 0; i < a.length; i++) {
    if (a[i] !== b[i]) return false;
  }
  return true;
}

/**
 * Field-level equality for one `TaskItem`, keyed to each variant's
 * OBSERVABLE fields: `state`/`manual` by value (fresh objects every
 * pass but the same content shouldn't count as a change), `row`/`pr`/
 * `slot` by identity (already reused upstream by `useWorktreeRows` /
 * the github query, so identity IS the change signal), everything else
 * by value. Backs the per-key cache in `useTaskRows` that keeps
 * `TaskItem` identity stable across recomputes so `memo(TaskRowView)`
 * can actually skip re-rendering unchanged rows.
 */
function taskItemEq(a: TaskItem, b: TaskItem): boolean {
  if (a.kind === "wt" && b.kind === "wt") {
    return (
      a.row === b.row &&
      taskStateEq(a.state, b.state) &&
      taskManualEq(a.manual, b.manual) &&
      a.detail === b.detail &&
      a.displayBucket === b.displayBucket &&
      a.inExpandedStack === b.inExpandedStack
    );
  }
  if (a.kind === "stack" && b.kind === "stack") {
    return (
      a.row === b.row &&
      taskStateEq(a.state, b.state) &&
      taskManualEq(a.manual, b.manual) &&
      a.detail === b.detail &&
      a.displayBucket === b.displayBucket &&
      a.label === b.label &&
      membersEq(a.members, b.members)
    );
  }
  if (a.kind === "pr" && b.kind === "pr") {
    return (
      a.pr === b.pr &&
      taskStateEq(a.state, b.state) &&
      a.detail === b.detail &&
      a.displayBucket === b.displayBucket
    );
  }
  if (a.kind === "slot" && b.kind === "slot") {
    return (
      a.slot === b.slot &&
      a.displayBucket === b.displayBucket &&
      a.collapsedGroup === b.collapsedGroup
    );
  }
  return false;
}

/**
 * Return `next` unless an item cached under the same key is field-equal
 * (`taskItemEq`) to it, in which case the CACHED reference is returned
 * instead — the reuse that lets `memo(TaskRowView)` skip a re-render.
 * Always (re-)stores whichever object wins under `next.key`, so the
 * cache tracks the latest live item per key.
 */
function reuseTaskItem(cache: Map<string, TaskItem>, next: TaskItem): TaskItem {
  const prev = cache.get(next.key);
  const out = prev && taskItemEq(prev, next) ? prev : next;
  cache.set(next.key, out);
  return out;
}

/**
 * Build the task-inbox list: every live (non-archived) worktree folded
 * into its stack (or standing alone), plus every review-request PR,
 * ranked by `compareTasks` and flattened to a flat render-ready list.
 *
 * Grouping/sorting strategy: compute each worktree row's derived task
 * data once, split rows into standalone vs. per-stack groups, build one
 * `SortUnit` per standalone row / stack / PR, sort the units, then
 * flatten — an expanded stack's `build()` emits multiple `TaskItem`s
 * but the unit itself sorts as one, so the group can never get split
 * apart by the sort.
 */
export function useTaskRows(opts: {
  rows: readonly WorktreeRow[];
  reviewRequests: readonly ReviewRequestPr[];
  activeSessionBySlug: ReadonlyMap<string, ActiveSessionGlyph>;
  activeActions: ReadonlySet<string>;
  wtState: WtState | undefined;
  stackSectionLabels: ReadonlyMap<string, string>;
  /** Stack section keys the user expanded (Tab). */
  expandedStacks: ReadonlySet<string>;
  /** Whether the bottom Sessions group shows all three slots (Tab). */
  slotsExpanded: boolean;
  isLoading: boolean;
  /**
   * True once the github query has completed a live fetch this
   * session — same meaning (and same source: `useAutomations`'
   * query-cache subscription) as `AutomationEvalCtx.githubFresh` in
   * `tui/automation-rules.ts`. Gates every PR-derived signal so a
   * task never buckets off yesterday's persisted PR cache.
   */
  githubFresh: boolean;
}): TaskRowsResult {
  const {
    rows,
    reviewRequests,
    activeSessionBySlug,
    activeActions,
    wtState,
    stackSectionLabels,
    expandedStacks,
    slotsExpanded,
    isLoading,
    githubFresh,
  } = opts;

  // `useSyncExternalStore` is the store's contract for React consumers
  // (see `core/task-focus.ts`); the snapshot identity only changes on a
  // real `record()`, so this doesn't force a rebuild on every render.
  const focusSnapshot = useSyncExternalStore(
    taskFocusStore.subscribe,
    taskFocusStore.getSnapshot,
  );

  // Per-key `TaskItem` cache + the previous flattened array, mirroring
  // `useWorktreeRows`' `rowCache`/`rowsRef` pattern (see
  // `useWorktreeRows.ts` around its `rows` memo): lets unchanged items
  // — and the whole array, when nothing changed — keep their identity
  // across recomputes so `memo(TaskRowView)` can skip re-rendering rows
  // whose observable fields didn't move.
  const itemCache = useRef<Map<string, TaskItem>>(new Map());
  const prevTasksRef = useRef<readonly TaskItem[]>([]);

  const tasks = useMemo(() => {
    // Hub shows the live inbox only — archived worktrees stay out of
    // it entirely (classic mode's list still shows them).
    const live = rows.filter((r) => !r.archived);

    const rowTasks = new Map<string, RowTaskData>();
    for (const row of live) {
      rowTasks.set(
        row.wt.slug,
        computeRowTask(row, activeSessionBySlug, activeActions, wtState, focusSnapshot, githubFresh),
      );
    }

    // Split into standalone rows vs. per-stack member groups. Stack
    // membership + grouping key come straight from `row.stack` /
    // `row.section` — `useWorktreeRows` already resolved both from the
    // inferred stack index, so there's nothing left to infer here.
    const standalone: WorktreeRow[] = [];
    const stackGroups = new Map<string, WorktreeRow[]>();
    for (const row of live) {
      if (row.stack) {
        const key = row.section!; // stack rows always carry the synthetic stack section key
        const group = stackGroups.get(key);
        if (group) group.push(row);
        else stackGroups.set(key, [row]);
      } else {
        standalone.push(row);
      }
    }
    // Spine order within each stack.
    for (const group of stackGroups.values()) {
      group.sort((a, b) => a.stack!.index - b.stack!.index);
    }

    const units: SortUnit[] = [];

    for (const row of standalone) {
      const rt = rowTasks.get(row.wt.slug)!;
      const displayBucket = displayBucketFor(rt.state, rt.manual);
      units.push({
        sortKey: { state: rt.state, manual: rt.manual, lastActivityMs: rt.lastActivityMs },
        build: () => [
          reuseTaskItem(itemCache.current, {
            kind: "wt",
            key: row.wt.slug,
            row,
            state: rt.state,
            manual: rt.manual,
            detail: rt.detail,
            displayBucket,
            inExpandedStack: false,
          }),
        ],
      });
    }

    for (const [key, members] of stackGroups) {
      const focus = pickFocusMember(
        members,
        (m) => rowTasks.get(m.wt.slug)!.state.bucket,
        (m) => m.stack!.ordinal,
      );
      const focusTask = rowTasks.get(focus.wt.slug)!;
      const label = stackSectionLabels.get(key) ?? key;

      // Effective manual overlay for the STACK'S sort position and
      // section grouping only — NOT the focus member's own `manual`/
      // `state` fields carried on the built `TaskItem` below, which
      // still drive the `z`/Shift+P toggles and the row's own badges
      // (`hub-keys.ts` reads `task.manual`/`task.state` and always
      // targets the focus slug). Two rules, folded over every live
      // member:
      //  - pinned: ANY member pinned promotes the whole stack — using
      //    only the focus slice's own pin here would mean pinning a
      //    non-focus member does nothing.
      //  - snoozed: only when EVERY member is live-snoozed at its OWN
      //    bucket. An individually-snoozed urgent focus member must
      //    NOT demote the whole stack below an unsnoozed lesser
      //    sibling — "any member snoozed" would do exactly that
      //    whenever the loudest member happened to be the snoozed one.
      const effectivePinned = members.some((m) => rowTasks.get(m.wt.slug)!.manual.pinned);
      const effectiveSnoozed = members.every((m) => rowTasks.get(m.wt.slug)!.state.snoozed);
      const sortState: TaskState = { ...focusTask.state, snoozed: effectiveSnoozed };
      const sortManual: TaskManual = { ...focusTask.manual, pinned: effectivePinned };
      const displayBucket = displayBucketFor(sortState, sortManual);
      const sortKey = {
        state: sortState,
        manual: sortManual,
        lastActivityMs: focusTask.lastActivityMs,
      };

      if (expandedStacks.has(key)) {
        // Expanded: splice in every member as its own item, in spine
        // order, inheriting the unit's displayBucket so the group never
        // straddles a section divider — but the unit as a whole still
        // sorts on the focus slice, so the spine stays contiguous.
        units.push({
          sortKey,
          build: () =>
            members.map((m) => {
              const rt = rowTasks.get(m.wt.slug)!;
              return reuseTaskItem(itemCache.current, {
                kind: "wt",
                key: m.wt.slug,
                row: m,
                state: rt.state,
                manual: rt.manual,
                detail: rt.detail,
                displayBucket,
                inExpandedStack: true,
              });
            }),
        });
      } else {
        units.push({
          sortKey,
          build: () => [
            reuseTaskItem(itemCache.current, {
              kind: "stack",
              key,
              row: focus,
              state: focusTask.state,
              manual: focusTask.manual,
              detail: focusTask.detail,
              displayBucket,
              label,
              members,
            }),
          ],
        });
      }
    }

    for (const pr of reviewRequests) {
      // Not derived through `deriveTaskState` — review-request PRs have
      // no worktree, hence no signals to fold; the bucket is a direct
      // draft/non-draft split per the spec.
      // Draft → waiting is a deliberate judgment call: the query is
      // explicit review-requested:@me, so a human DID ask — but a
      // draft signals the author isn't actually ready, and surfacing
      // it needs-you would nag before there's anything reviewable.
      const state: TaskState = pr.isDraft
        ? { bucket: "waiting", reason: "draft review request", snoozed: false }
        : { bucket: "needs-you", reason: "review requested", snoozed: false };
      const manual: TaskManual = { pinned: false, snoozedBucket: null };
      const lastActivityMs = Date.parse(pr.updatedAt) || 0;
      const detail = `${pr.author ?? "?"} · +${pr.additions} −${pr.deletions}`;
      const displayBucket = state.bucket; // never pinned/snoozed
      units.push({
        sortKey: { state, manual, lastActivityMs },
        build: () => [
          reuseTaskItem(itemCache.current, {
            kind: "pr",
            key: pr.url,
            pr,
            state,
            detail,
            displayBucket,
          }),
        ],
      });
    }

    units.sort((a, b) => compareTasks(a.sortKey, b.sortKey));

    // The Sessions group sits below everything, outside the bucket
    // sort — it's infrastructure, not work. Main clone leads (the one
    // you reach for most); Tab expands to the wt-source + dotfiles
    // slots.
    const visibleSlots = slotsExpanded
      ? [MAIN_CLONE_SLOT, WT_SOURCE_SLOT, DOTFILES_SLOT]
      : [MAIN_CLONE_SLOT];
    const slotItems: TaskItem[] = visibleSlots.map((slot) =>
      reuseTaskItem(itemCache.current, {
        kind: "slot",
        key: `slot:${slot.slug}`,
        slot,
        displayBucket: "sessions",
        collapsedGroup: !slotsExpanded,
      }),
    );

    const flat = [...units.flatMap((u) => u.build()), ...slotItems];

    // Prune cache entries for keys no longer present, same rationale
    // as `useWorktreeRows`' `rowCache` prune: without this the map
    // grows unboundedly across the session as worktrees/PRs/stacks
    // come and go.
    if (itemCache.current.size > flat.length) {
      const liveKeys = new Set(flat.map((t) => t.key));
      for (const k of itemCache.current.keys()) {
        if (!liveKeys.has(k)) itemCache.current.delete(k);
      }
    }

    // Keep the final array's own identity stable when every element
    // was reused — lets a consumer that depends on `tasks` (e.g. the
    // scroll-follow effect in `panels/tasks.tsx`) skip work on a pass
    // where nothing observable changed at all.
    const prevTasks = prevTasksRef.current;
    let tasksUnchanged = prevTasks.length === flat.length;
    if (tasksUnchanged) {
      for (let i = 0; i < flat.length; i++) {
        if (prevTasks[i] !== flat[i]) {
          tasksUnchanged = false;
          break;
        }
      }
    }
    const result = tasksUnchanged ? prevTasks : flat;
    prevTasksRef.current = result;
    return result;
  }, [
    rows,
    reviewRequests,
    activeSessionBySlug,
    activeActions,
    wtState,
    stackSectionLabels,
    expandedStacks,
    slotsExpanded,
    focusSnapshot,
    githubFresh,
  ]);

  return { tasks, isLoading };
}

/**
 * Resolve a persisted selection key (`sel`, from `taskFocusStore` or
 * wherever the caller tracks "last selected task") to an index into
 * the current `tasks` array. Exact rules, in order:
 *
 *  1. Direct `key` match (the common case — the exact same item is
 *     still present).
 *  2. Otherwise `sel` may be a worktree SLUG that has since moved
 *     inside a collapsed stack (`row.section === sel` on an expanded
 *     member) or that names a collapsed stack's key directly
 *     (`row.section === sel`) — or `sel` may itself be a stack section
 *     key whose collapsed item's members still include it.
 *  3. Falls back to `0` (the top of the inbox) when nothing matches,
 *     or when `sel` is `null`.
 *
 * Pure and side-effect-free so it's usable both from the app's
 * selection-derivation memo and from unit tests, without constructing
 * a `TaskRowsResult` at all.
 */
export function resolveTaskIndex(tasks: readonly TaskItem[], sel: string | null): number {
  if (sel === null) return 0;
  const direct = tasks.findIndex((t) => t.key === sel);
  if (direct >= 0) return direct;
  const bySlug = tasks.findIndex((t) =>
    t.kind === "wt"
      ? t.row.wt.slug === sel || t.row.section === sel
      : t.kind === "stack"
        ? t.members.some((m) => m.wt.slug === sel)
        : false,
  );
  return bySlug >= 0 ? bySlug : 0;
}
