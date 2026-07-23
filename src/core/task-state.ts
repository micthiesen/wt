/**
 * Pure task-inbox derivation for the hub UI: maps a worktree's (or
 * stack's, or review-request PR's) normalized signals to a single
 * prioritized bucket. This is the "what does this task need from me
 * right now" triage that turns the worktree list into an inbox.
 *
 * PURE by design — no queries, no fs, no tui/ imports. The hub layer
 * is responsible for gathering `TaskSignals` from the real sources
 * (session registry, git, github) and for persisting `TaskManual`;
 * this module only knows how to fold them into a bucket + reason, and
 * how to order tasks once bucketed. Keeping it pure means the whole
 * precedence table is unit-testable without touching a single query
 * or the filesystem.
 */
import type { DerivedState } from "./harness/status.ts";

export type TaskBucket =
  | "needs-you" // top: human input required right now
  | "review-output" // agent finished a turn you haven't looked at since
  | "ready" // PR approved + green: land it
  | "working" // something is actively running on your behalf
  | "waiting" // blocked on others (review, merge queue, CI)
  | "idle"
  | "done"; // branch landed; sweep with `c`

/** Canonical priority order — top of the inbox to bottom. */
export const TASK_BUCKET_ORDER: readonly TaskBucket[] = [
  "needs-you",
  "review-output",
  "ready",
  "working",
  "waiting",
  "idle",
  "done",
];

export const TASK_BUCKET_LABEL: Record<TaskBucket, string> = {
  "needs-you": "Needs you",
  "review-output": "Review output",
  ready: "Ready to land",
  working: "Working",
  waiting: "Waiting on others",
  idle: "Idle",
  done: "Done",
};

export type TaskManual = {
  pinned: boolean;
  /** Bucket the user snoozed this task at; stale (≠ current bucket) means the snooze expired. */
  snoozedBucket: TaskBucket | null;
};

export type TaskPrSignals = {
  state: "OPEN" | "CLOSED" | "MERGED";
  isDraft: boolean;
  checks: "passing" | "failing" | "pending" | "none";
  reviewDecision: "APPROVED" | "CHANGES_REQUESTED" | "REVIEW_REQUIRED" | null;
  autoMergeArmed: boolean;
  inMergeQueue: boolean;
};

export type TaskSignals = {
  sessionState: DerivedState | null; // null = no session known
  /** ts(ms) of the last jsonl entry IF the turn is over (session waiting/idle/abandoned), else null */
  turnEndedAt: number | null;
  /** when the user last focused this task's session in the hub; null = never */
  lastFocusedAt: number | null;
  busyLock: boolean; // per-slug op lock held (create/remove/restack)
  actionRunning: boolean; // tracked headless action in flight
  conflict: boolean; // merge-tree probe says branch conflicts with its base
  midRebase: boolean; // a rebase is sitting in the worktree (conflict stop)
  dirty: boolean;
  mergedOrGone: boolean; // branch landed upstream (clean candidate)
  pr: TaskPrSignals | null;
  lastActivityMs: number; // recency for intra-bucket ordering
};

export type TaskState = {
  bucket: TaskBucket;
  /** Short lowercase reason phrase for the row's status line, e.g. "agent is asking", "checks failing". */
  reason: string;
  /** True when manual.snoozedBucket === bucket (the snooze is live). */
  snoozed: boolean;
};

/** True when `pr` exists and is still open (draft or not). Every rung
 *  below that reads PR state needs this same guard, so it's factored
 *  out rather than repeated at each call site. */
function isOpenPr(pr: TaskPrSignals | null): pr is TaskPrSignals {
  return pr !== null && pr.state === "OPEN";
}

/**
 * First-match-wins precedence ladder from the module doc comment.
 * Each rung is a self-contained predicate + reason pair; order here
 * IS the spec, so don't reorder without re-reading the precedence
 * table this mirrors.
 */
function computeBucket(sig: TaskSignals): { bucket: TaskBucket; reason: string } {
  const pr = sig.pr;

  // 1. done — landed, one way or another. Checked first so a merged
  // PR can't get stuck reporting a stale "needs-you"/"working" state
  // from before it landed.
  if (sig.mergedOrGone || pr?.state === "MERGED") {
    return { bucket: "done", reason: "merged" };
  }

  // 2. needs-you — something requires a human RIGHT NOW.
  if (sig.sessionState === "asking") {
    return { bucket: "needs-you", reason: "agent is asking" };
  }
  if (sig.midRebase) {
    return { bucket: "needs-you", reason: "conflict mid-rebase" };
  }
  if (sig.conflict) {
    return { bucket: "needs-you", reason: "merge conflict with base" };
  }
  if (isOpenPr(pr) && pr.checks === "failing") {
    return { bucket: "needs-you", reason: "checks failing" };
  }
  if (isOpenPr(pr) && !pr.isDraft && pr.reviewDecision === "CHANGES_REQUESTED") {
    return { bucket: "needs-you", reason: "changes requested" };
  }

  // 3. working — something is actively running on the user's behalf.
  if (sig.busyLock) {
    return { bucket: "working", reason: "worktree busy" };
  }
  if (sig.actionRunning) {
    return { bucket: "working", reason: "action running" };
  }
  if (sig.sessionState === "working") {
    return { bucket: "working", reason: "agent working" };
  }
  if (sig.sessionState === "polling") {
    return { bucket: "working", reason: "agent polling a task" };
  }
  if (isOpenPr(pr) && pr.autoMergeArmed && pr.checks === "pending") {
    return { bucket: "working", reason: "auto-merge armed · ci running" };
  }

  // 4. review-output — a turn ended and the user hasn't looked since.
  // `turnEndedAt` is only ever non-null when the turn is actually over
  // (per its field contract), so no need to re-check sessionState here.
  if (
    sig.turnEndedAt !== null &&
    (sig.lastFocusedAt === null || sig.turnEndedAt > sig.lastFocusedAt)
  ) {
    return { bucket: "review-output", reason: "unreviewed agent output" };
  }

  // 5. ready — approved and green, just waiting to be landed.
  if (
    isOpenPr(pr) &&
    !pr.isDraft &&
    pr.reviewDecision === "APPROVED" &&
    (pr.checks === "passing" || pr.checks === "none")
  ) {
    return { bucket: "ready", reason: "approved · green" };
  }

  // 6. waiting — blocked on others.
  if (pr?.inMergeQueue) {
    return { bucket: "waiting", reason: "in merge queue" };
  }
  if (isOpenPr(pr) && !pr.isDraft && pr.checks === "pending") {
    return { bucket: "waiting", reason: "ci running" };
  }
  if (
    isOpenPr(pr) &&
    !pr.isDraft &&
    (pr.reviewDecision === null || pr.reviewDecision === "REVIEW_REQUIRED")
  ) {
    return { bucket: "waiting", reason: "awaiting review" };
  }

  // 7. idle — nothing pending. `dirty` and "abandoned" are just
  // reason-text refinements of the same bucket, not separate rungs:
  // an abandoned session whose output was already seen has nothing
  // left to review, it's just an idle worktree with history.
  if (sig.dirty) {
    return { bucket: "idle", reason: "dirty" };
  }
  if (sig.sessionState === "abandoned") {
    return { bucket: "idle", reason: "abandoned agent session" };
  }
  return { bucket: "idle", reason: "idle" };
}

export function deriveTaskState(sig: TaskSignals, manual: TaskManual): TaskState {
  const { bucket, reason } = computeBucket(sig);
  return { bucket, reason, snoozed: manual.snoozedBucket === bucket };
}

export type TaskSortKey = {
  state: TaskState;
  manual: TaskManual;
  lastActivityMs: number;
};

/**
 * Rank a bucket for sorting, with a synthetic slot for live-snoozed
 * tasks between "idle" and "done" — snoozing doesn't change what a
 * task *is* (the bucket field is untouched), it just demotes it out
 * of the way until the snooze goes stale (bucket changes underneath
 * it) or the user un-snoozes it.
 */
function rankForSort(state: TaskState): number {
  const idleIdx = TASK_BUCKET_ORDER.indexOf("idle");
  if (state.snoozed) return idleIdx + 0.5;
  return TASK_BUCKET_ORDER.indexOf(state.bucket);
}

/** Pinned first; then bucket order (a live-snoozed task ranks after "idle" but before "done"); then lastActivityMs desc. */
export function compareTasks(a: TaskSortKey, b: TaskSortKey): number {
  if (a.manual.pinned !== b.manual.pinned) {
    return a.manual.pinned ? -1 : 1;
  }
  const rankDiff = rankForSort(a.state) - rankForSort(b.state);
  if (rankDiff !== 0) return rankDiff;
  return b.lastActivityMs - a.lastActivityMs;
}
