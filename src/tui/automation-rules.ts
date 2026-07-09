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
 * key encodes the failure INSTANCE — head SHA for push-scoped
 * conditions — so the same failure never re-fires but a new push does.
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
        `ci:${slug}:${pr.headRefOid}`,
        `checks failing on #${pr.number} (${names})`,
      );
    }
    case "rabbit.unresolved": {
      const pr = freshOpenPr(row, ctx);
      if (!pr || pr.rabbit.state !== "unresolved") return null;
      return singleRowFire(
        rule,
        row,
        `rabbit:${slug}:${pr.headRefOid}`,
        `${pluralize(pr.rabbit.unresolved, "unresolved carrot")} on #${pr.number}`,
      );
    }
    case "review.changes_requested": {
      const pr = freshOpenPr(row, ctx);
      if (!pr || pr.review !== "changes_requested") return null;
      return singleRowFire(
        rule,
        row,
        `review:${slug}:${pr.headRefOid}`,
        `changes requested on #${pr.number}`,
      );
    }
    case "pr.conflict": {
      const conflict = row.fields.conflict;
      if (conflict.isLoading || conflict.data?.status !== "conflict") return null;
      // The probe is computed locally (never persisted), so no boot-
      // staleness gate. The head oid rides in when a PR exists so a
      // conflict that reappears after a fixing push re-fires; without a
      // PR the base alone has to do.
      const head = row.pr?.headRefOid ?? "local";
      const base = conflict.data.base;
      return singleRowFire(
        rule,
        row,
        `conflict:${slug}:${base}:${head}`,
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
        `merged:${slug}:${row.pr?.number ?? "local"}`,
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
 * `stack.parent_merged`: a stack has ≥1 merged slice with ≥1 open slice
 * remaining. One fire per stack, keyed per merged parent PR so a second
 * parent landing later re-fires. Targets the first open slice (event
 * attribution + action target); quiesces the WHOLE stack since the
 * restack pre-cleans the merged slices and replays everything else.
 *
 * Only sees merged parents that still have a live worktree row — a
 * parent already cleaned by hand is invisible here, but its bookkeeping
 * lands via `reconcileStack` inside the next replay anyway.
 */
function evaluateStackTrigger(
  rule: AutomationDef,
  rows: readonly WorktreeRow[],
  ctx: AutomationEvalCtx,
): AutomationFire[] {
  const byStack = new Map<string, WorktreeRow[]>();
  for (const row of rows) {
    if (!row.stack || row.stack.isHolistic) continue;
    if (!isEligible(row, ctx)) continue;
    const arr = byStack.get(row.stack.stackId);
    if (arr) arr.push(row);
    else byStack.set(row.stack.stackId, [row]);
  }
  const fires: AutomationFire[] = [];
  for (const [stackId, members] of byStack) {
    const merged = members.filter(
      (r) =>
        isCleanCandidate(r) &&
        // The PR-merged leg needs fresh github data; merged/gone are local.
        (r.pr?.state !== "MERGED" || ctx.githubFresh),
    );
    const open = members.filter((r) => !merged.includes(r));
    if (merged.length === 0 || open.length === 0) continue;
    fires.push({
      rule,
      slug: open[0]!.wt.slug,
      quiesceSlugs: members.map((r) => r.wt.slug),
      fireKeys: merged.map(
        (r) => `restack:${stackId}:${r.pr?.number ?? r.wt.branch}`,
      ),
      stackId,
      detail: `${pluralize(merged.length, "merged slice")} under ${pluralize(open.length, "open slice")}`,
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
