import type { AutomationDef } from "../core/config.ts";
import {
  evaluateAutomations,
  fireIdentity,
  type AutomationEvalCtx,
  type AutomationFire,
} from "./automation-rules.ts";
import type { WorktreeRow } from "./hooks/useWorktreeRows.ts";

/** Pausing drops the dispatch queue, but must not require resuming it to cancel. */
export function cancellableAutomationFires(opts: {
  paused: boolean;
  stateReady: boolean;
  rules: readonly AutomationDef[];
  rows: readonly WorktreeRow[];
  evalCtx: AutomationEvalCtx;
  pending: readonly AutomationFire[];
  executing: ReadonlySet<string>;
  handled: (key: string) => boolean;
}): AutomationFire[] {
  if (!opts.stateReady) return [];
  // Reconstruct only while globally paused. The evaluator still enforces
  // freshness, per-worktree pauses and row eligibility; no dispatch gates or
  // ledger writes run here. Existing frozen intents may outlive their rows.
  const candidates = opts.paused
    ? [...opts.pending, ...evaluateAutomations(opts.rules, opts.rows, opts.evalCtx)]
    : opts.pending;
  const fires = new Map<string, AutomationFire>();
  for (const fire of candidates) {
    const id = fireIdentity(fire);
    if (opts.executing.has(id)) continue;
    const fireKeys = fire.fireKeys.filter((key) => !opts.handled(key));
    if (fireKeys.length === 0) continue;
    const prior = fires.get(id);
    fires.set(id, {
      ...fire,
      fireKeys: [...new Set([...(prior?.fireKeys ?? []), ...fireKeys])],
    });
  }
  return [...fires.values()];
}
