import type { RequireTag } from "../config.ts";
import { isTrackerIssueId, resolveIssueId } from "../issue-tracker.ts";
import type { ActionAvailability, ActionRowState } from "./types.ts";

/**
 * Evaluate a def's `requires` against a row snapshot. Pure synchronous
 * fn: predicates read cached row state, so optimistic patches that
 * already mutated row state cascade for free (mark a PR ready → next
 * picker open shows `requires = ["pr.ready"]` actions as available
 * before the server confirms; rollback re-blocks them). See the
 * architecture block in `state/hooks.ts`.
 */
export function evaluateActionRequirements(
  requires: readonly RequireTag[],
  row: ActionRowState,
): ActionAvailability {
  for (const req of requires) {
    switch (req) {
      case "pr":
        if (!row.pr) return { ok: false, reason: "no PR" };
        break;
      case "pr.ready":
        if (!row.pr) return { ok: false, reason: "no PR" };
        if (row.pr.isDraft) return { ok: false, reason: "PR is draft" };
        if (row.pr.state !== "OPEN") return { ok: false, reason: "PR not open" };
        break;
      case "deployed":
        if (!row.deployed) return { ok: false, reason: "no stage deployed" };
        break;
      // An action templated on `{{issue_id}}` renders an EMPTY string
      // for a slug carrying no tracker id, so without this it runs a
      // command with a hole in its argument list — which fails, but as
      // a usage error naming nothing the reader can act on. Worse
      // where it is bound to an automation: `wt.merged` holds for
      // every landing, so a fleet whose worktrees are keyed to GitHub
      // issues rather than tracker tasks (0 of 6 live rows here) earns
      // a red attention line on every single merge. A guard that cries
      // wolf on 100% of a population is not a guard.
      //
      // Unlike `pr` and `deployed` this one is not waiting on the
      // world to change: it is true or false about the worktree's
      // identity. It IS fixable though — `wt issue <slug> --id COZ-123`
      // (TUI `I`) stores an override — so the reason names the remedy
      // rather than just the lack, since a permanently-grayed picker
      // entry with no way out is the same as a missing feature.
      case "issue.tracker":
        if (!isTrackerIssueId(resolveIssueId(row.slug, row.issueId))) {
          return { ok: false, reason: "no tracker id (set one with `#`)" };
        }
        break;
      default: {
        // Exhaustiveness check, adding a new RequireTag without
        // updating this switch is a type error. Critical because the
        // failure mode is silent always-allow (worse than always-block).
        const _exhaustive: never = req;
        throw new Error(`unhandled require tag: ${String(_exhaustive)}`);
      }
    }
  }
  return { ok: true };
}
