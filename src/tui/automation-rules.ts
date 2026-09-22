/**
 * Pure condition evaluation for `[[automations]]` — the "when should a
 * rule fire, against which worktree, under which once-only keys" half
 * of the engine. No IO, no ledger access, no dispatch: it maps the
 * current aggregated row state to a list of `AutomationFire`s, and the
 * hook (`hooks/useAutomations.ts`) decides what to do with them
 * (dedupe against the ledger, queue, deliver).
 *
 * Triggers are LEVEL conditions, deliberately: "checks are failing" is
 * re-derivable at any time, which is what makes the engine restart-safe
 * (a condition that arose while wt was closed still fires once on next
 * boot) and lets the intent queue be pure derived state.
 *
 * # Freshness guard
 *
 * PR-driven conditions only evaluate when the github query has been
 * live-fetched THIS SESSION (`githubFresh`) and the PR carries a
 * `headRefOid`. The persisted cache restores yesterday's data at boot —
 * firing "fix CI" off a stale red badge is exactly the class of bug the
 * guard exists to prevent (the ledger would dedupe a re-fire, but not a
 * first-fire on dead data). Locally-computed fields (conflict probe,
 * merged/gone) only need their own query to have loaded.
 */
import { config } from "../core/config.ts";
import type { AutomationDef, AutomationTrigger } from "../core/config.ts";
import type { ActionVars } from "../core/actions.ts";
import { githubIssueNumberFromSlug } from "../core/issue-tracker.ts";
import { MANAGER_SLUG } from "../core/manager.ts";
import { pluralize } from "../core/text.ts";
import { REVIEW_BOT_NONE, StatusKind } from "../core/types.ts";
import {
  isGated,
  verificationOverdue,
  workStatusSuffix,
  type WorkState,
} from "../core/work-status.ts";
import { dayBucketFromMs } from "./day-headers.ts";

import { isCleanCandidate } from "./app-helpers.ts";
import type { WorktreeRow } from "./hooks/useWorktreeRows.ts";

/**
 * Target of a fleet-level fire — one that belongs to no worktree. Leads
 * with `_` because a slug cannot: `wt new` derives slugs from branch
 * names, so no real row can ever collide with this one.
 */
export const FLEET_SLUG = "_fleet";

export type AutomationFire = {
  rule: AutomationDef;
  /** Worktree the run targets (for stack triggers: the first open member). */
  slug: string;
  /**
   * Every slug that must be quiescent before delivery. Single-worktree
   * triggers list just the target; `stack.parent_merged` lists every
   * live member since a restack rebases (and pre-cleans) all of them.
   */
  quiesceSlugs: readonly string[];
  /**
   * Once-only ledger keys. Usually one; `stack.parent_merged` carries
   * one per merged parent PR so a later second merge re-fires. A fire
   * is fresh while ANY key is unseen; dispatch records ALL of them.
   */
  fireKeys: readonly string[];
  /** Stack id (root branch) for `builtin:restack` dispatches; null otherwise. */
  stackId: string | null;
  /**
   * Issue number a `builtin:close-issue` run closes, FROZEN at fire
   * time from the row; null for every other run. Frozen so delivery
   * depends on nothing surviving: the row is routinely destroyed
   * before dispatch (a clean/restack pre-clean, a manual `c` inside
   * the settle window), and a wtstate re-read at delivery could even
   * hit a recreated slug's fresh state and close the wrong issue.
   */
  closeIssue: number | null;
  /**
   * Branch a `builtin:delete-branch` run deletes on the origin repo,
   * FROZEN at fire time; null for every other run. Frozen for the
   * same reason as `closeIssue`, and it matters more here: a stale
   * re-read at delivery would delete a ref, which nothing in wt can
   * undo.
   */
  deleteBranch: string | null;
  /**
   * The PR whose merge this `builtin:delete-branch` fire is claiming,
   * or null when the evidence was local containment (branch landed on
   * trunk, no PR). Frozen alongside `deleteBranch` so the dispatch can
   * CONFIRM the claim against GitHub before deleting a ref, instead of
   * trusting the condition that produced the fire.
   *
   * The fire's condition can be wrong: the merged/gone answers are
   * cached and were once keyed on the slug alone, so a repointed row
   * served the previous branch's verdict and this rule deleted the
   * remote ref of an open PR — GitHub then closed that PR as a side
   * effect of its head ref vanishing, with no close event to explain
   * it. Keying those queries on the branch fixes the cause; confirming
   * here is what makes the consequence unreachable from any future
   * cause, because deleting a ref is the one thing in this file wt
   * cannot undo.
   */
  deleteBranchPr: number | null;
  /**
   * `branch.advanced` only: the branch and the commit range its tip
   * moved across, FROZEN at fire time for the same reason `closeIssue`
   * is — the watermark advances on dispatch, so a re-read at delivery
   * would compute a range that has already been consumed.
   */
  branchRange: { branch: string; from: string; to: string } | null;
  /**
   * Template vars for a post-merge EXTERNAL shell run, FROZEN at fire
   * time; null for every other run — and its non-nullness is what
   * marks the fire as one, so the dispatcher needs no second predicate.
   *
   * Same freeze as `closeIssue`/`deleteBranch` and for the same
   * reason, which merely bites harder here because there are eight
   * values rather than one: the row a merge fires on is routinely
   * destroyed inside the settle window (the `c` sweep archived one 7.5
   * seconds after the merge landed, against a 10-second window), and
   * an action whose vars are re-read at delivery has nothing to read.
   * Frozen, the run needs no checkout at all, which is why it can go
   * to the main clone.
   */
  frozenVars: ActionVars | null;
  /** PR requirement evidence captured with a post-merge external fire. */
  frozenPr?: Pick<NonNullable<WorktreeRow["pr"]>, "state" | "isDraft">;
  /** Human-readable trigger summary for the activity-pane event line. */
  detail: string;
};

/**
 * The asserted work state a `status.*` trigger fires on, or null for
 * every other trigger. Shared with the breaker-reset logic in
 * `useAutomations`, which must distinguish "condition actually
 * cleared" (state changed) from "row merely ineligible this pass"
 * (busy/paused) — resetting on the latter would wipe legitimate
 * breaker counts.
 */
export function statusTriggerState(trigger: AutomationTrigger): WorkState | null {
  switch (trigger) {
    case "status.needs_human":
      return "needs-human";
    case "status.needs_testing":
      return "needs-testing";
    case "status.ready":
      return "ready";
    default:
      return null;
  }
}

/**
 * Which live conversation a rule's dispatch lands in: the singleton
 * manager session, the target worktree's own session, or nobody
 * durable (`headless` runs, and every builtin — a notification, a
 * clean, a restack has no audience to echo back at).
 */
export type FireAudience = "manager" | "session" | null;

export type AutomationEvalCtx = {
  /**
   * True once the github query has completed a live fetch this session
   * (`dataUpdatedAt` past app start). Gates every PR-derived condition.
   */
  githubFresh: boolean;
  /** Per-worktree pause flag (Ctrl+A), read from wtstate. */
  isPausedSlug: (slug: string) => boolean;
  /**
   * Where this rule's run would be delivered — resolved from the
   * `[[actions]]` def's `target` by the hook, which owns the config.
   */
  audienceOf: (rule: AutomationDef) => FireAudience;
  /**
   * True when this rule runs a SHELL action declared `external = true`
   * — its effect leaves the repository, so it needs nothing from the
   * checkout and must outlive it. Resolved by the hook for the same
   * reason `audienceOf` is: config ownership lives there.
   *
   * Deliberately shell-only. A prompt action can be `external` too (it
   * posts somewhere), but it is delivered INTO a session in the
   * worktree, so the checkout is exactly what it does need.
   */
  externalOf: (rule: AutomationDef) => boolean;
  /** Template vars for a row, frozen into an external fire. */
  varsFor: (rule: AutomationDef, row: WorktreeRow) => ActionVars;
  /**
   * Current tip of each branch a `branch.advanced` rule watches, and
   * the last tip wt recorded for it (absent on first sight). Resolved
   * by the caller in the MAIN CLONE — "where the world is now" is a
   * question about the clone that fetches, never about a checkout.
   */
  branchTips: ReadonlyMap<string, { now: string; seen: string | null }>;
  /**
   * Wall clock for this pass. Passed in rather than read here so the
   * module stays a pure function of its inputs — `status.verification_overdue`
   * is the first condition whose truth depends on the time of day
   * (its fire key carries the local day), and a rule engine that
   * cannot be evaluated twice with the same answer cannot be tested.
   */
  nowMs: number;
};

/**
 * True when the session a fire would brief is the same one that wrote
 * the status triggering it. That is not a briefing, it is an echo: the
 * manager's last triage step is sharpening the needs-human note it was
 * briefed about, which re-asserts the state and — with the fire key
 * carrying the assertion timestamp — briefs it again, quoting its own
 * words back and asking it to triage them. Observed three times for one
 * slug, and the honest answer to the third was "nothing changed".
 *
 * The cost is not the wasted turn. A briefing whose correct answer is
 * usually "nothing changed" stops getting read, which spends the one
 * channel that exists for "a worktree is blocked on the human".
 *
 * Only status triggers can loop this way: every other condition is
 * derived from git/GitHub, which no session writes by asserting.
 */
function writerIsAudience(
  by: string | null,
  slug: string,
  audience: FireAudience,
): boolean {
  if (!by) return false;
  if (audience === "manager") return by === MANAGER_SLUG;
  if (audience === "session") return by === slug;
  return false;
}

/**
 * A row the engine may evaluate at all: live (not archived — archived
 * rows opted out of the automatic lifecycle, same as `c`), not mid
 * destroy/init, and not individually paused.
 *
 * Exported for the breaker-reset pass in `useAutomations`: an absent
 * fire for an INELIGIBLE row means "not evaluated", never "condition
 * cleared", so resets must skip these rows or a transient Busy lock /
 * Ctrl+A toggle hands a flapping fix-loop free strikes.
 */
export function isEligible(row: WorktreeRow, ctx: AutomationEvalCtx): boolean {
  if (row.archived) return false;
  if (row.status.kind === StatusKind.Busy) return false;
  if (ctx.isPausedSlug(row.wt.slug)) return false;
  return true;
}

/** Fresh open PR with a head oid, or null when not evaluable. */
function freshOpenPr(row: WorktreeRow, ctx: AutomationEvalCtx) {
  if (!ctx.githubFresh) return null;
  const pr = row.pr;
  if (!pr || pr.state !== "OPEN" || !pr.headRefOid) return null;
  return pr;
}

function singleRowFire(
  rule: AutomationDef,
  row: WorktreeRow,
  fireKey: string,
  detail: string,
): AutomationFire {
  return {
    rule,
    slug: row.wt.slug,
    quiesceSlugs: [row.wt.slug],
    fireKeys: [fireKey],
    stackId: null,
    closeIssue: null,
    deleteBranch: null,
    deleteBranchPr: null,
    branchRange: null,
    frozenVars: null,
    detail,
  };
}

/**
 * Evaluate one single-worktree trigger against one row. Returns null
 * when the condition doesn't hold (or isn't evaluable yet). The fire
 * key encodes the RULE plus the failure INSTANCE — head SHA for
 * push-scoped conditions — so the same failure never re-fires the same
 * rule, a new push does, and two rules bound to the same trigger can't
 * starve each other by consuming a shared key.
 */
function evaluateRowTrigger(
  trigger: AutomationTrigger,
  rule: AutomationDef,
  row: WorktreeRow,
  ctx: AutomationEvalCtx,
): AutomationFire | null {
  const slug = row.wt.slug;
  switch (trigger) {
    case "wt.created": {
      // No retrofit from directory mtimes or first observation. An old checkout
      // must not suddenly become "started" merely because a rule was enabled.
      if (!row.createdAt || row.fields.merged.isLoading || row.fields.gone.isLoading) return null;
      if (row.fields.merged.data || row.fields.gone.data || row.pr?.state === "MERGED") return null;
      const fire = singleRowFire(rule, row, `${rule.id}:created:${slug}:${row.createdAt}`, "worktree created");
      // External bookkeeping needs no idle harness, but unlike merged fires it
      // does not survive removal: creating then deleting must not start a task.
      if (ctx.externalOf(rule)) fire.quiesceSlugs = [];
      return fire;
    }
    case "pr.checks.failed": {
      const pr = freshOpenPr(row, ctx);
      if (!pr || pr.checks !== "fail") return null;
      const failed = pr.failedChecks ?? [];
      const names = failed.length > 0 ? failed.join(", ") : "checks";
      return singleRowFire(
        rule,
        row,
        `${rule.id}:ci:${slug}:${pr.headRefOid}`,
        `checks failing on #${pr.number} (${names})`,
      );
    }
    case "review_bot.unresolved": {
      const pr = freshOpenPr(row, ctx);
      const rb = pr?.reviewBot ?? REVIEW_BOT_NONE;
      if (!pr || rb.state !== "unresolved") return null;
      // The ":rabbit:" segment predates the review_bot rename and is
      // FROZEN: the on-disk ledger keys existing fires with it, and
      // changing it would re-dispatch every already-handled fire once
      // after an upgrade. It's an opaque internal key — never displayed.
      return singleRowFire(
        rule,
        row,
        `${rule.id}:rabbit:${slug}:${pr.headRefOid}`,
        `${pluralize(rb.unresolved, "unresolved review-bot finding")} on #${pr.number}`,
      );
    }
    case "review.changes_requested": {
      if (!config.github.reviewers) return null;
      const pr = freshOpenPr(row, ctx);
      if (!pr || pr.review !== "changes_requested") return null;
      return singleRowFire(
        rule,
        row,
        `${rule.id}:review:${slug}:${pr.headRefOid}`,
        `changes requested on #${pr.number}`,
      );
    }
    case "pr.conflict": {
      const conflict = row.fields.conflict;
      if (conflict.isLoading || conflict.data?.status !== "conflict") return null;
      // The probe is computed locally (never persisted), so it needs no
      // boot-staleness gate of its own — but the fire key does. With a
      // PR, the head oid is the instance marker (a conflict that
      // reappears after a fixing push re-fires), so wait for a live oid
      // rather than baking a stale persisted one into the key (which
      // would double-fire the same conflict once the live oid lands).
      // Without a PR the base alone has to do.
      if (row.pr && (!ctx.githubFresh || !row.pr.headRefOid)) return null;
      const head = row.pr?.headRefOid ?? "local";
      const base = conflict.data.base;
      return singleRowFire(
        rule,
        row,
        `${rule.id}:conflict:${slug}:${base}:${head}`,
        `conflicts with ${base.replace(/^origin\//, "")}`,
      );
    }
    case "wt.merged": {
      const closesIssue = rule.run === "builtin:close-issue";
      const deletesBranch = rule.run === "builtin:delete-branch";
      // A config shell action declared `external = true` is the same
      // KIND of thing as those two builtins — it writes outside the
      // repository and reads nothing from the checkout — and it was
      // being treated as the opposite until the fleet noticed that not
      // one tracker transition had fired in three days. Same three
      // properties, so the same three exemptions below.
      const externalRun = !closesIssue && !deletesBranch && ctx.externalOf(rule);
      // Non-stacked worktrees only — merged stack members are cleaned
      // by the stack.parent_merged → builtin:restack path, and letting
      // both fire would race a clean against a whole-stack restack.
      // Exception: runs that never touch the WORKTREE have no race to
      // protect against and would otherwise miss every stacked landing,
      // so they evaluate stack members too. Deleting a merged parent's
      // remote ref is safe for its children on both sides: GitHub
      // retargets an open child PR onto the deleted base's own base,
      // and wt's restack replays from the `baseSha` anchor in wtstate,
      // never from the remote ref.
      if (row.stack && !closesIssue && !deletesBranch && !externalRun) return null;
      // The PR-merged leg of isCleanCandidate needs fresh github data;
      // the merged/gone legs are local. Split the check accordingly.
      const localDone =
        (!row.fields.merged.isLoading && row.fields.merged.data === true) ||
        (!row.fields.gone.isLoading && row.fields.gone.data === true);
      const prDone = ctx.githubFresh && row.pr?.state === "MERGED";
      if (!localDone && !prDone) return null;
      if (!isCleanCandidate(row)) return null;
      // Name the evidence that actually fired, not the PR that happens
      // to be on the row. `#N merged` was emitted whenever a PR
      // existed, including when the merged verdict came from the LOCAL
      // leg — so wt asserted "#1631 merged" in its own audit trail
      // about a PR opened three minutes earlier and never merged,
      // which is what made the incident undiagnosable from GitHub's
      // side and had two readers concluding a human closed it.
      const landed = prDone
        ? `#${row.pr!.number} merged`
        : row.pr
          ? `branch landed on trunk (#${row.pr.number} not observed merged)`
          : "branch landed on trunk";
      if (closesIssue) {
        // Only fire when there's actually an issue to close — a fire
        // with nothing behind it would still burn a ledger key and
        // narrate a run that does nothing. The number rides the fire
        // (`closeIssue`), frozen from this evaluation.
        const issue = row.githubIssue ?? githubIssueNumberFromSlug(slug);
        if (issue === null) return null;
        return {
          rule,
          slug,
          // Nothing needs to be quiescent to close an issue, and the
          // empty set keeps this dispatch from blocking — or being
          // blocked by — a clean or restack racing on the same row
          // (the restack pre-clean destroys merged members; if this
          // fire had to wait its turn on the slug, the row could be
          // gone and the intent dropped as superseded before delivery).
          quiesceSlugs: [],
          fireKeys: [`${rule.id}:merged:${slug}:${row.pr?.number ?? "local"}`],
          stackId: null,
          closeIssue: issue,
          deleteBranch: null,
          deleteBranchPr: null,
    branchRange: null,
    frozenVars: null,
          detail: `${landed} — closing issue #${issue}`,
        };
      }
      if (deletesBranch) {
        // The branch rides the fire for the same reason the issue
        // number does: the row is routinely destroyed before dispatch
        // (a clean, a restack pre-clean, a manual `c`), and re-reading
        // at delivery could hit a recreated slug and delete the wrong
        // ref. Quiescence is empty for the same reason too — this
        // touches GitHub, never the checkout.
        const branch = row.wt.branch;
        if (!branch || branch === config.branch.base) return null;
        return {
          rule,
          slug,
          quiesceSlugs: [],
          fireKeys: [`${rule.id}:merged:${slug}:${row.pr?.number ?? "local"}`],
          stackId: null,
          closeIssue: null,
          deleteBranch: branch,
          deleteBranchPr: row.pr?.number ?? null,
          branchRange: null,
          frozenVars: null,
          detail: `${landed} — deleting remote branch ${branch}`,
        };
      }
      if (externalRun) {
        return {
          rule,
          slug,
          // Empty for the same reason the two builtins' is: this
          // touches GitHub or a ticket tracker, never the checkout, so
          // there is nothing to be quiescent about — and waiting its
          // turn on the slug is precisely how it lost. It queued
          // behind the row's own lifetime and the `c` sweep archived
          // the row 7.5s later, inside the 10s merge settle window, so
          // the intent was dropped as superseded and the ticket never
          // moved.
          quiesceSlugs: [],
          fireKeys: [`${rule.id}:merged:${slug}:${row.pr?.number ?? "local"}`],
          stackId: null,
          closeIssue: null,
          deleteBranch: null,
          deleteBranchPr: null,
          branchRange: null,
          // Everything the command needs, taken while the row is still
          // here. `{{issue_id}}` above all: it is the whole point of
          // the run and is unrecoverable once the worktree is gone.
          frozenVars: ctx.varsFor(rule, row),
          frozenPr: row.pr ? { state: row.pr.state, isDraft: row.pr.isDraft } : undefined,
          detail: landed,
        };
      }
      return singleRowFire(
        rule,
        row,
        `${rule.id}:merged:${slug}:${row.pr?.number ?? "local"}`,
        landed,
      );
    }
    case "status.needs_human":
    case "status.needs_testing":
    case "status.ready": {
      // Work-status assertions are local (wtstate) — no freshness gate.
      // The fire key carries the assertion timestamp: one fire per
      // assertion, and re-asserting (new `at`) legitimately re-fires —
      // unless the asserter is the session this rule would brief, which
      // is an echo rather than news (see `writerIsAudience`).
      const want = statusTriggerState(trigger)!;
      const work = row.work;
      if (!work || work.state !== want) return null;
      // A gated `ready` is finished but must not be merged, and every
      // documented use of `status.ready` says "look at me, this is
      // yours to merge" — firing it here would reproduce the exact
      // misread at the escalation layer, where it is loudest. The fire
      // comes back on its own when the gate clears: `--unblock` amends
      // in place, so `at` is unchanged and the fire key was never
      // consumed.
      if (isGated(work)) return null;
      if (writerIsAudience(work.by ?? null, slug, ctx.audienceOf(rule))) return null;
      return singleRowFire(
        rule,
        row,
        `${rule.id}:work:${slug}:${work.at}`,
        `${want}${workStatusSuffix(work)}`,
      );
    }
    case "status.verification_overdue": {
      // Local (wtstate + the row's own merged/gone signals), so no
      // freshness gate — but `rowHasLanded` also accepts a MERGED PR,
      // and that leg is github-derived. Gate only that one: a
      // boot-stale cache would otherwise nag about a branch whose
      // merge it has not confirmed this session.
      const landed =
        row.status.kind === StatusKind.Merged ||
        row.status.kind === StatusKind.Gone ||
        (ctx.githubFresh && row.pr?.state === "MERGED");
      if (!verificationOverdue(row.work, landed, ctx.nowMs, rule.afterDays)) {
        return null;
      }
      // The one fire key here that is not once-per-instance. The
      // instance is "this obligation, today": it re-fires each local
      // day until someone asserts `verified`, because the failure mode
      // is a check nobody runs and a single reminder that scrolled off
      // is indistinguishable from no reminder at all. It ends the
      // moment the obligation is discharged — nothing the recipient
      // writes can silence it, which is what separates this from the
      // needs-human echo the `by` stamp exists to stop.
      const day = dayBucketFromMs(ctx.nowMs);
      return singleRowFire(
        rule,
        row,
        `${rule.id}:unverified:${slug}:${day}`,
        `merged, verification still owed — ${row.work!.verifyAfterMerge!}`,
      );
    }
    case "stack.parent_merged":
      // Stack-level; handled in evaluateStackTrigger.
      return null;
    case "branch.advanced":
      // Fleet-level; handled in evaluateBranchTrigger. It targets no
      // row on purpose — the worktrees whose work is in the range are
      // gone by the time a release branch moves, which is the whole
      // reason the trigger is not per-row.
      return null;
    default: {
      const _exhaustive: never = trigger;
      void _exhaustive;
      return null;
    }
  }
}

/**
 * `stack.parent_merged`: a stack needs a restack because a parent
 * landed. Three parent shapes, unioned into one fire per stack:
 *
 *  - an in-stack member merged with open members stacked on it (keyed
 *    per merged parent PR so a second landing later re-fires);
 *  - an external parent merged — an open member whose recorded base
 *    branch lives outside this stack's members and whose row shows
 *    merged. The restack's `reconcileStack` reparents the member, and
 *    the pre-clean covers the parent's worktree;
 *  - an external parent with NO live worktree row at all (`extgone`) —
 *    covers the race where the parent was cleaned before this fire
 *    delivered, and heals pre-existing stale boundaries at boot. Fires
 *    once per (stack, branch); the reconcile probes the branch's PR
 *    directly and no-ops when the parent is actually still open, so a
 *    false positive costs one idle reconcile+replay.
 *
 * Targets the first open member (event attribution + action target);
 * quiesces the whole stack plus any merged external parent, since the
 * restack pre-cleans those and replays everything else.
 */
function evaluateStackTrigger(
  rule: AutomationDef,
  rows: readonly WorktreeRow[],
  ctx: AutomationEvalCtx,
): AutomationFire[] {
  const byStack = new Map<string, WorktreeRow[]>();
  // Stacks with ANY individually-paused member are skipped entirely —
  // a restack rebases the whole stack, so a Ctrl+A on one member must
  // protect it from sibling-triggered fires too, not just its own.
  const pausedStacks = new Set<string>();
  for (const row of rows) {
    if (!row.stack) continue;
    if (ctx.isPausedSlug(row.wt.slug)) {
      pausedStacks.add(row.stack.stackId);
      continue;
    }
    if (!isEligible(row, ctx)) continue;
    const arr = byStack.get(row.stack.stackId);
    if (arr) arr.push(row);
    else byStack.set(row.stack.stackId, [row]);
  }
  const fires: AutomationFire[] = [];
  const rowBySlug = new Map(rows.map((r) => [r.wt.slug, r] as const));
  for (const [stackId, members] of byStack) {
    if (pausedStacks.has(stackId)) continue;
    const merged = members.filter(
      (r) =>
        isCleanCandidate(r) &&
        // The PR-merged leg needs fresh github data; merged/gone are local.
        (r.pr?.state !== "MERGED" || ctx.githubFresh),
    );
    const open = members.filter((r) => !merged.includes(r));
    if (open.length === 0) continue;
    // Cross-stack boundary: open members whose recorded base branch is
    // outside this stack's members. `stackedOn` is already resolved against
    // the live worktree list (slug null = no worktree for that branch).
    const memberBranches = new Set(members.map((r) => r.wt.branch));
    const extMerged: WorktreeRow[] = [];
    const extGone: string[] = [];
    const seenParents = new Set<string>();
    for (const m of open) {
      const so = m.stackedOn;
      if (!so || memberBranches.has(so.branch) || seenParents.has(so.branch)) {
        continue;
      }
      seenParents.add(so.branch);
      if (so.slug === null) {
        extGone.push(so.branch);
        continue;
      }
      const parentRow = rowBySlug.get(so.slug);
      if (!parentRow || parentRow.archived) continue;
      if (parentRow.status.kind === StatusKind.Busy) continue;
      // A paused parent means hands-off its worktree AND the boundary —
      // whoever paused it is mid-surgery there.
      if (ctx.isPausedSlug(parentRow.wt.slug)) continue;
      if (
        isCleanCandidate(parentRow) &&
        (parentRow.pr?.state !== "MERGED" || ctx.githubFresh)
      ) {
        extMerged.push(parentRow);
      }
    }
    const fireKeys = [
      ...merged.map(
        (r) => `${rule.id}:restack:${stackId}:${r.pr?.number ?? r.wt.branch}`,
      ),
      ...extMerged.map(
        (r) => `${rule.id}:restack:${stackId}:ext:${r.pr?.number ?? r.wt.branch}`,
      ),
      ...extGone.map((b) => `${rule.id}:restack:${stackId}:extgone:${b}`),
    ];
    if (fireKeys.length === 0) continue;
    const parts: string[] = [];
    if (merged.length > 0) parts.push(pluralize(merged.length, "merged member"));
    if (extMerged.length > 0) {
      parts.push(
        `merged external parent ${extMerged.map((r) => (r.pr ? `#${r.pr.number}` : r.wt.branch)).join(", ")}`,
      );
    }
    if (extGone.length > 0) {
      parts.push(`external parent gone (${extGone.join(", ")})`);
    }
    fires.push({
      rule,
      slug: open[0]!.wt.slug,
      quiesceSlugs: [
        ...members.map((r) => r.wt.slug),
        ...extMerged.map((r) => r.wt.slug),
      ],
      fireKeys,
      stackId,
      closeIssue: null,
      deleteBranch: null,
      deleteBranchPr: null,
    branchRange: null,
    frozenVars: null,
      detail: `${parts.join(" + ")} under ${pluralize(open.length, "open member")}`,
    });
  }
  return fires;
}

/**
 * Full evaluation pass: every rule against every eligible row (or
 * stack). Pure — same inputs, same fires — which is what makes the
 * intent queue re-derivable after a restart and lets the hook diff
 * consecutive passes to detect superseded intents and breaker resets.
 */
export function evaluateAutomations(
  rules: readonly AutomationDef[],
  rows: readonly WorktreeRow[],
  ctx: AutomationEvalCtx,
): AutomationFire[] {
  if (rules.length === 0) return [];
  const fires: AutomationFire[] = [];
  for (const rule of rules) {
    if (rule.on === "stack.parent_merged") {
      fires.push(...evaluateStackTrigger(rule, rows, ctx));
      continue;
    }
    if (rule.on === "branch.advanced") {
      const fire = evaluateBranchTrigger(rule, ctx);
      if (fire) fires.push(fire);
      continue;
    }
    for (const row of rows) {
      if (!isEligible(row, ctx)) continue;
      const fire = evaluateRowTrigger(rule.on, rule, row, ctx);
      if (fire) fires.push(fire);
    }
  }
  return fires;
}

/**
 * `branch.advanced`: a watched branch's tip moved.
 *
 * Fleet-level, so it produces at most one fire per rule per pass and
 * targets `FLEET_SLUG` rather than a worktree — every row that
 * contributed to the range has usually been swept by now.
 *
 * FIRST SIGHT FIRES NOTHING. With no recorded tip there is no range,
 * and the tempting reading of an absent watermark ("everything up to
 * here") would fire once for the entire history of the branch — which
 * for the run this exists for means marking every issue ever shipped.
 * The caller records the tip instead, and the next move produces a real
 * range. Absence is unknown, never all.
 */
function evaluateBranchTrigger(
  rule: AutomationDef,
  ctx: AutomationEvalCtx,
): AutomationFire | null {
  const branch = rule.branch;
  if (!branch) return null;
  const tip = ctx.branchTips.get(branch);
  if (!tip || !tip.seen || tip.seen === tip.now) return null;
  return {
    rule,
    slug: FLEET_SLUG,
    // Nothing to quiesce: the run touches no worktree, and waiting on a
    // fleet to fall idle would mean a busy fleet never releases.
    quiesceSlugs: [],
    // Keyed on the DESTINATION sha: one fire per tip, and a branch that
    // moves again while this one is still pending gets its own.
    fireKeys: [`${rule.id}:branch:${branch}:${tip.now}`],
    stackId: null,
    closeIssue: null,
    deleteBranch: null,
    deleteBranchPr: null,
    branchRange: { branch, from: tip.seen, to: tip.now },
    frozenVars: null,
    detail: `${branch} advanced ${tip.seen.slice(0, 7)}..${tip.now.slice(0, 7)}`,
  };
}

/** Stable identity for an intent: one live intent per (rule, target). */
export function fireIdentity(fire: AutomationFire): string {
  return `${fire.rule.id}|${fire.stackId ?? fire.slug}`;
}
