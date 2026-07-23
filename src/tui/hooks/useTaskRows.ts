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
import { useMemo, useSyncExternalStore } from "react";

import type { SessionTail } from "../../core/harness/claude/jsonl.ts";
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
import type { ActiveSessionGlyph } from "./useHarnessSessions.ts";
import type { WorktreeRow } from "./useWorktreeRows.ts";

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
      displayBucket: string;
      inExpandedStack: boolean;
    }
  | {
      kind: "stack";
      key: string /* stack section key */;
      row: WorktreeRow /* focus slice */;
      state: TaskState;
      manual: TaskManual;
      detail: string | null;
      displayBucket: string;
      label: string;
      members: readonly WorktreeRow[];
    }
  | {
      kind: "pr";
      key: string /* pr.url */;
      pr: ReviewRequestPr;
      state: TaskState;
      detail: string | null;
      displayBucket: string;
    };

export type TaskRowsResult = { tasks: readonly TaskItem[]; isLoading: boolean };

/**
 * Session tail with the greatest `lastEntryMs` across a worktree's
 * claude sessions (primary + named). This is the row's single "most
 * recently active conversation" — `turnEndedAt`, `lastActivityMs`, and
 * `detail` (pendingAsk / lastAssistantText) all read off it rather than
 * re-scanning the session list per signal.
 */
function freshestTail(sessions: readonly SessionTail[] | undefined): SessionTail | null {
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
function prChecksToTaskChecks(c: PrChecks): TaskPrSignals["checks"] {
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
function mapReviewDecision(r: PrReview): TaskPrSignals["reviewDecision"] {
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

/** Null when the row has no PR at all — `deriveTaskState`'s `isOpenPr` guard handles the rest. */
function buildPrSignals(
  pr: PullRequest | undefined,
  mq: MergeQueueEntry | undefined,
): TaskPrSignals | null {
  if (!pr) return null;
  return {
    state: pr.state,
    isDraft: pr.isDraft,
    checks: prChecksToTaskChecks(pr.checks),
    reviewDecision: mapReviewDecision(pr.review),
    autoMergeArmed: pr.autoMerge !== null,
    inMergeQueue: mq != null,
  };
}

function isTaskBucket(x: string): x is TaskBucket {
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
function displayBucketFor(state: TaskState, manual: TaskManual): string {
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
): RowTaskData {
  const slug = row.wt.slug;
  const tail = freshestTail(row.fields.claude.data?.sessions);
  const sessionState = activeSessionBySlug.get(slug)?.state ?? null;
  // Only meaningful when the freshest tail's own terminal-ness agrees
  // with the live session state — a session that's since resumed
  // working past an old end_turn/paused tail shouldn't still read as
  // "turn ended" just because the jsonl hasn't caught up.
  const turnEndedAt =
    tail !== null &&
    tail.lastEntryMs !== null &&
    (tail.lastEntryKind === "end_turn" || tail.lastEntryKind === "paused") &&
    sessionState !== "working" &&
    sessionState !== "polling" &&
    sessionState !== "asking"
      ? tail.lastEntryMs
      : null;
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
    pr: buildPrSignals(row.pr, row.mq),
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
  isLoading: boolean;
}): TaskRowsResult {
  const {
    rows,
    reviewRequests,
    activeSessionBySlug,
    activeActions,
    wtState,
    stackSectionLabels,
    expandedStacks,
    isLoading,
  } = opts;

  // `useSyncExternalStore` is the store's contract for React consumers
  // (see `core/task-focus.ts`); the snapshot identity only changes on a
  // real `record()`, so this doesn't force a rebuild on every render.
  const focusSnapshot = useSyncExternalStore(
    taskFocusStore.subscribe,
    taskFocusStore.getSnapshot,
  );

  const tasks = useMemo(() => {
    // Hub shows the live inbox only — archived worktrees stay out of
    // it entirely (classic mode's list still shows them).
    const live = rows.filter((r) => !r.archived);

    const rowTasks = new Map<string, RowTaskData>();
    for (const row of live) {
      rowTasks.set(
        row.wt.slug,
        computeRowTask(row, activeSessionBySlug, activeActions, wtState, focusSnapshot),
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
          {
            kind: "wt",
            key: row.wt.slug,
            row,
            state: rt.state,
            manual: rt.manual,
            detail: rt.detail,
            displayBucket,
            inExpandedStack: false,
          },
        ],
      });
    }

    for (const [key, members] of stackGroups) {
      // Focus slice: the member whose bucket ranks highest (lowest
      // TASK_BUCKET_ORDER index = most urgent); ties broken by spine
      // ordinal so the pick is deterministic. Uses the raw bucket rank,
      // not the snooze-adjusted sort rank — a snoozed member "winning"
      // the focus slice would hide a genuinely urgent stack behind its
      // own snooze.
      let focus = members[0]!;
      let focusRank = TASK_BUCKET_ORDER.indexOf(rowTasks.get(focus.wt.slug)!.state.bucket);
      for (let i = 1; i < members.length; i++) {
        const m = members[i]!;
        const rank = TASK_BUCKET_ORDER.indexOf(rowTasks.get(m.wt.slug)!.state.bucket);
        if (rank < focusRank || (rank === focusRank && m.stack!.ordinal < focus.stack!.ordinal)) {
          focus = m;
          focusRank = rank;
        }
      }
      const focusTask = rowTasks.get(focus.wt.slug)!;
      const label = stackSectionLabels.get(key) ?? key;
      const displayBucket = displayBucketFor(focusTask.state, focusTask.manual);
      const sortKey = {
        state: focusTask.state,
        manual: focusTask.manual,
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
              return {
                kind: "wt",
                key: m.wt.slug,
                row: m,
                state: rt.state,
                manual: rt.manual,
                detail: rt.detail,
                displayBucket,
                inExpandedStack: true,
              };
            }),
        });
      } else {
        units.push({
          sortKey,
          build: () => [
            {
              kind: "stack",
              key,
              row: focus,
              state: focusTask.state,
              manual: focusTask.manual,
              detail: focusTask.detail,
              displayBucket,
              label,
              members,
            },
          ],
        });
      }
    }

    for (const pr of reviewRequests) {
      // Not derived through `deriveTaskState` — review-request PRs have
      // no worktree, hence no signals to fold; the bucket is a direct
      // draft/non-draft split per the spec.
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
          {
            kind: "pr",
            key: pr.url,
            pr,
            state,
            detail,
            displayBucket,
          },
        ],
      });
    }

    units.sort((a, b) => compareTasks(a.sortKey, b.sortKey));
    return units.flatMap((u) => u.build());
  }, [
    rows,
    reviewRequests,
    activeSessionBySlug,
    activeActions,
    wtState,
    stackSectionLabels,
    expandedStacks,
    focusSnapshot,
  ]);

  return { tasks, isLoading };
}
