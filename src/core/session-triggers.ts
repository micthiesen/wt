/**
 * Refresh triggers for the interactive Claude session tail.
 *
 * `session-tail.ts` already tails every live wt-managed `claude` jsonl
 * for the activity pane. This module is the "while we're reading the
 * stream anyway" addition: scan each new entry for Bash tool calls
 * that change *remote* state we can't cheaply poll — chiefly GitHub —
 * and report a refresh target so the tailer can invalidate the
 * matching query immediately instead of waiting out its slow
 * staleTime.
 *
 * Deliberately small, not comprehensive: it's an optimization, not a
 * correctness mechanism. A missed trigger just means the user waits
 * for the next poll. Local git state (HEAD, branch, dirty tree) is
 * already cheaply polled, so it isn't worth a trigger — the value is
 * specifically the GitHub state behind a rate-limited API.
 *
 * Claude-only for now. Codex / OpenCode expose tool calls too (their
 * event pollers already parse them), so extending later is cheap, but
 * there's no second implementation yet to justify the seam.
 */

/** A query family the tailer can ask the runtime to invalidate. */
export type RefreshTarget = "github";

type TriggerRule = {
  /** Tested against a Bash tool call's `command` string. */
  readonly match: RegExp;
  readonly target: RefreshTarget;
};

/**
 * Match on the structured Bash `command`, never on Claude's prose —
 * "Opened PR #123" is phrased a hundred ways, `gh pr create` is not.
 * Substring matches are fine: a compound command (`cat > body <<EOF
 * … EOF; gh pr create …`) still carries the verbatim invocation, and
 * a false positive (the string inside an `echo`) costs only a spare
 * query invalidation.
 */
const TRIGGER_RULES: readonly TriggerRule[] = [
  // PR lifecycle — create / merge / ready / edit / close / reopen all
  // change GitHub-side state the slow `["github"]` poll won't surface
  // for a while. review / comment cover reviews submitted from a
  // session (e.g. /review-pr), which move PRs out of the
  // review-requests section.
  {
    match: /\bgh\s+pr\s+(?:create|merge|ready|edit|close|reopen|review|comment)\b/,
    target: "github",
  },
  // A push starts CI remotely; the checks badge should refetch.
  { match: /\bgit\s+push\b/, target: "github" },
];

function isObj(v: unknown): v is Record<string, unknown> {
  return v !== null && typeof v === "object" && !Array.isArray(v);
}

/**
 * Scan one raw jsonl line for Bash tool calls that warrant a state
 * refresh. Only `assistant` envelopes carry `tool_use` blocks; every
 * other entry shape (and any parse failure) returns an empty set.
 *
 * Matches on `tool_use` (the call), not `tool_result` (the
 * completion): the caller debounces the resulting refresh by a few
 * seconds, which both coalesces bursts and gives the command time to
 * finish before we refetch — without the bookkeeping a tool_use →
 * tool_result correlation would need.
 */
export function detectRefreshTriggers(raw: string): Set<RefreshTarget> {
  const out = new Set<RefreshTarget>();
  let entry: unknown;
  try {
    entry = JSON.parse(raw);
  } catch {
    return out;
  }
  if (!isObj(entry) || entry.type !== "assistant") return out;
  const message = isObj(entry.message) ? entry.message : null;
  const content =
    message && Array.isArray(message.content) ? message.content : null;
  if (!content) return out;
  for (const block of content) {
    if (!isObj(block) || block.type !== "tool_use" || block.name !== "Bash") {
      continue;
    }
    const input = isObj(block.input) ? block.input : null;
    const command =
      input && typeof input.command === "string" ? input.command : null;
    if (!command) continue;
    for (const rule of TRIGGER_RULES) {
      if (rule.match.test(command)) out.add(rule.target);
    }
  }
  return out;
}
