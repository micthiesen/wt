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
import type { AutomationDef, AutomationTrigger } from "../core/config.ts";
import { pluralize } from "../core/text.ts";
import { StatusKind } from "../core/types.ts";

import { isCleanCandidate } from "./app-helpers.ts";
import type { WorktreeRow } from "./hooks/useWorktreeRows.ts";

export type AutomationFire = {
  rule: AutomationDef;
  /** Worktree the run targets (for stack triggers: the first open slice). */
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
  /** Stack id for `builtin:restack` dispatches; null otherwise. */
  stackId: string | null;
  /** Human-readable trigger summary for the activity-pane event line. */
  detail: string;
};

export type AutomationEvalCtx = {
  /**
   * True once the github query has completed a live fetch this session
   * (`dataUpdatedAt` past app start). Gates every PR-derived condition.
   */
  githubFresh: boolean;
  /** Per-worktree pause flag (Ctrl+A), read from wtstate. */
  isPausedSlug: (slug: string) => boolean;
};

/**
 * A row the engine may evaluate at all: live (not archived — archived
 * rows opted out of the automatic lifecycle, same as `c`), not mid
 * destroy/init, and not individually paused.
 */
function isEligible(row: WorktreeRow, ctx: AutomationEvalCtx): boolean {
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
    case "rabbit.unresolved": {
      const pr = freshOpenPr(row, ctx);
      if (!pr || pr.rabbit.state !== "unresolved") return null;
      return singleRowFire(
        rule,
        row,
        `${rule.id}:rabbit:${slug}:${pr.headRefOid}`,
        `${pluralize(pr.rabbit.unresolved, "unresolved carrot")} on #${pr.number}`,
      );
    }
    case "review.changes_requested": {
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
      // Non-stack worktrees only — merged stack slices are cleaned by
      // the stack.parent_merged → builtin:restack path, and letting
      // both fire would race a clean against a whole-stack restack.
      if (row.stack) return null;
      // The PR-merged leg of isCleanCandidate needs fresh github data;
      // the merged/gone legs are local. Split the check accordingly.
      const localDone =
        (!row.fields.merged.isLoading && row.fields.merged.data === true) ||
        (!row.fields.gone.isLoading && row.fields.gone.data === true);
      const prDone = ctx.githubFresh && row.pr?.state === "MERGED";
      if (!localDone && !prDone) return null;
      if (!isCleanCandidate(row)) return null;
      return singleRowFire(
        rule,
        row,
        `${rule.id}:merged:${slug}:${row.pr?.number ?? "local"}`,
        row.pr ? `#${row.pr.number} merged` : "branch landed on trunk",
      );
    }
    case "stack.parent_merged":
      // Stack-level; handled in evaluateStackTrigger.
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
 *  - an in-manifest slice merged with open slices remaining (keyed per
 *    merged parent PR so a second landing later re-fires);
 *  - a STACK-ON-STACK external parent merged — an open slice whose
 *    resolved base branch lives outside this manifest (another stack's
 *    tip, or a plain PR branch) and whose row shows merged. The
 *    restack's `reconcileStack` external-parent pass reparents the
 *    slice onto trunk, and the pre-clean covers the parent's worktree
 *    (its own stack's manifest gets reconciled by the clean flow);
 *  - an external parent with NO live worktree row at all (`extgone`) —
 *    covers the race where the parent was cleaned before this fire
 *    delivered, and heals pre-existing stale boundaries at boot. Fires
 *    once per (stack, branch); the reconcile probes the branch's PR
 *    directly and no-ops when the parent is actually still open, so a
 *    false positive costs one idle reconcile+replay.
 *
 * Targets the first open slice (event attribution + action target);
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
  // a restack rebases the whole stack, so a Ctrl+A on one slice must
  // protect it from sibling-triggered fires too, not just its own.
  const pausedStacks = new Set<string>();
  for (const row of rows) {
    if (!row.stack || row.stack.isHolistic) continue;
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
    // Cross-stack boundary: open slices whose resolved base branch is
    // outside this manifest. `stackedOn` is already resolved against
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
    if (merged.length > 0) parts.push(pluralize(merged.length, "merged slice"));
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
      detail: `${parts.join(" + ")} under ${pluralize(open.length, "open slice")}`,
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
    for (const row of rows) {
      if (!isEligible(row, ctx)) continue;
      const fire = evaluateRowTrigger(rule.on, rule, row, ctx);
      if (fire) fires.push(fire);
    }
  }
  return fires;
}

/** Stable identity for an intent: one live intent per (rule, target). */
export function fireIdentity(fire: AutomationFire): string {
  return `${fire.rule.id}|${fire.stackId ?? fire.slug}`;
}
