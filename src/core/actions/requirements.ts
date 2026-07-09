import type { RequireTag } from "../config.ts";
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
