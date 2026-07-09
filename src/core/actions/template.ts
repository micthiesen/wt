import type { ActionVars } from "./types.ts";

/**
 * Sed-style template renderer. Replaces `{{name}}` with `vars[name]`;
 * unknown vars pass through unchanged so a typo is visible in the
 * launched prompt (and in the action log header) rather than silently
 * collapsing to an empty string.
 *
 * NO shell escaping is applied (audited, accepted): shell actions run
 * the rendered string via `$SHELL -lc`, so a var value with
 * metacharacters (a branch name containing `;`, a doctored
 * `.sst/stage`) would execute. Every substituted value in this
 * single-operator tool is the operator's own — the exposure is
 * checking out a hostile foreign branch and then running a shell
 * action that interpolates `{{branch}}`, which we accept. If wt ever
 * takes multi-user input, quote each value (`shQuote` in tmux.ts)
 * before substitution instead of re-deriving this conclusion.
 */
export function applyVars(template: string, vars: ActionVars): string {
  return template.replaceAll(/\{\{(\w+)\}\}/g, (m, k) => vars[k] ?? m);
}
