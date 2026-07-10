import { config } from "../config.ts";
import type { PrChecks, PrComment, PrReview, PullRequest, RabbitStatus } from "../types.ts";
import type {
  GqlCommentAuthor,
  GqlPrNode,
  GqlReviewDecision,
  GqlReviewThread,
  RawCheck,
} from "./types.ts";

const CHECK_FAIL_CONCLUSIONS = new Set([
  "FAILURE",
  "CANCELLED",
  "TIMED_OUT",
  "ACTION_REQUIRED",
  "STARTUP_FAILURE",
]);

/** Compile `[github] ignored_checks` (case-insensitive globs, `*` only) into a predicate. */
function compileIgnore(patterns: readonly string[]): (name: string | null | undefined) => boolean {
  if (patterns.length === 0) return () => false;
  const regexes = patterns.map((p) => {
    const escaped = p.replace(/[.+?^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*");
    return new RegExp(`^${escaped}$`, "i");
  });
  return (name) => {
    if (!name) return false;
    return regexes.some((r) => r.test(name));
  };
}

const isIgnoredCheck = compileIgnore(config.github.ignoredChecks);

function checkName(c: RawCheck): string | null | undefined {
  return c.__typename === "CheckRun" ? c.name : c.context;
}

export function rollupChecks(raw: RawCheck[] | null | undefined): PrChecks {
  if (!raw || raw.length === 0) return "none";
  let pending = false;
  let fail = false;
  let counted = 0;
  for (const c of raw) {
    if (isIgnoredCheck(checkName(c))) continue;
    counted++;
    if (c.__typename === "CheckRun") {
      if (c.status && c.status !== "COMPLETED") pending = true;
      else if (c.conclusion && CHECK_FAIL_CONCLUSIONS.has(c.conclusion)) fail = true;
    } else {
      const s = c.state;
      if (s === "PENDING" || s === "EXPECTED") pending = true;
      else if (s === "FAILURE" || s === "ERROR") fail = true;
    }
  }
  if (counted === 0) return "none";
  if (fail) return "fail";
  if (pending) return "pending";
  return "pass";
}

/**
 * Names of the checks currently failing, using the same fail set +
 * ignore filter as `rollupChecks`. Order preserved from the rollup;
 * powers the details-pane "which checks failed" line and the
 * `--log-failed` tail keybind. Empty unless something actually failed.
 */
function failingCheckNames(raw: RawCheck[] | null | undefined): string[] {
  if (!raw) return [];
  const out: string[] = [];
  for (const c of raw) {
    const name = checkName(c);
    if (isIgnoredCheck(name)) continue;
    const failed =
      c.__typename === "CheckRun"
        ? !!(c.conclusion && CHECK_FAIL_CONCLUSIONS.has(c.conclusion))
        : c.state === "FAILURE" || c.state === "ERROR";
    if (failed && name) out.push(name);
  }
  return out;
}

/**
 * Floor an OPEN PR's check rollup at `pending`. GitHub reports an empty
 * rollup in the window between opening a PR and the first check run
 * registering (and momentarily during some background refreshes), which
 * `rollupChecks` maps to `none`. In this workflow every open PR runs CI, so
 * `none` on an open PR means "checks haven't reported yet", not "no CI" —
 * surface it as `pending` so the badge holds its slot instead of vanishing
 * and shoving the rest of the row's glyph cluster around on each refresh.
 * Closed/merged PRs keep `none` (their check state is genuinely terminal).
 */
export function openPrChecks(state: PullRequest["state"], checks: PrChecks): PrChecks {
  return state === "OPEN" && checks === "none" ? "pending" : checks;
}

// CodeRabbit's status-check context name and GraphQL author login. Both
// are user-facing strings owned by CR, not us — if they change, the
// rabbit badge silently disappears (state: "none") rather than breaking
// the whole pane. Hardcoded by design; mirrors `~/.claude/skills/rabbit`.
const CR_CONTEXT = "CodeRabbit";
const CR_LOGIN = "coderabbitai";

function rollupRabbit(
  state: PullRequest["state"],
  contexts: RawCheck[] | null | undefined,
  threads: GqlReviewThread[] | null | undefined,
): RabbitStatus {
  if (state !== "OPEN") return { state: "none", unresolved: 0 };

  let unresolved = 0;
  for (const t of threads ?? []) {
    if (t.isResolved) continue;
    if (t.comments.nodes[0]?.author?.login === CR_LOGIN) unresolved++;
  }
  // Unresolved feedback takes precedence over a fresh re-run — pushes
  // re-trigger CR routinely and the old threads are still what needs
  // addressing.
  if (unresolved > 0) return { state: "unresolved", unresolved };

  // Find CR's status-check context to distinguish "still running" from
  // "done with no findings" from "never ran / not configured".
  let crContextState: "pending" | "done" | null = null;
  for (const c of contexts ?? []) {
    const name = c.__typename === "CheckRun" ? c.name : c.context;
    if (name !== CR_CONTEXT) continue;
    if (c.__typename === "CheckRun") {
      crContextState = c.status && c.status !== "COMPLETED" ? "pending" : "done";
    } else {
      crContextState = c.state === "PENDING" || c.state === "EXPECTED" ? "pending" : "done";
    }
    break;
  }
  if (crContextState === "pending") return { state: "pending", unresolved: 0 };
  if (crContextState === "done") return { state: "clean", unresolved: 0 };
  return { state: "none", unresolved: 0 };
}

function rollupReview(
  state: PullRequest["state"],
  decision: GqlReviewDecision,
  outstandingRequests: number,
  hasStaleChangesRequest: boolean,
): PrReview {
  // Terminal PRs don't carry useful review state; suppress to keep the
  // line uncluttered. The PR badge already conveys merged/closed.
  if (state !== "OPEN") return "none";
  if (decision === "APPROVED") return "approved";
  if (decision === "CHANGES_REQUESTED") {
    // GitHub keeps `reviewDecision` pinned at CHANGES_REQUESTED until
    // the same reviewer submits a new review or the old one is
    // dismissed — re-requesting review doesn't clear it. When the
    // reviewer who CR'd is back in `reviewRequests`, the practical
    // state is "fixed it, asked again, waiting" so surface that as
    // pending instead of leaving the stale "changes requested" badge
    // up.
    if (hasStaleChangesRequest) return "pending";
    return "changes_requested";
  }
  // Distinguish "nobody asked yet" from "asked, waiting" — the action
  // for the first is to hit `v`, for the second it's to wait.
  if (outstandingRequests === 0) return "unrequested";
  return "pending";
}

/**
 * True when at least one reviewer whose latest review was
 * CHANGES_REQUESTED has been re-requested (their login is back in the
 * currently-requested set). Walks `reviews` newest-to-oldest and only
 * keeps each author's most recent terminal verdict so a CR followed by
 * a fresh APPROVED doesn't keep flagging the author.
 */
function hasStaleChangesRequest(
  reviews: GqlPrNode["reviews"],
  currentlyRequested: readonly string[],
): boolean {
  if (currentlyRequested.length === 0) return false;
  const requested = new Set(currentlyRequested);
  const seen = new Set<string>();
  const nodes = reviews?.nodes ?? [];
  for (let i = nodes.length - 1; i >= 0; i--) {
    const r = nodes[i];
    const author = r?.author?.login;
    if (!author || seen.has(author)) continue;
    // Only verdict-bearing states reset the author's running state.
    // COMMENTED and PENDING are non-terminal and should be skipped so
    // an idle "commented" after a CR doesn't mask the CR.
    if (r.state !== "APPROVED" && r.state !== "CHANGES_REQUESTED" && r.state !== "DISMISSED") {
      continue;
    }
    seen.add(author);
    if (r.state === "CHANGES_REQUESTED" && requested.has(author)) return true;
  }
  return false;
}

function extractRequestedReviewers(
  rr: GqlPrNode["reviewRequests"],
): string[] {
  const out: string[] = [];
  for (const node of rr?.nodes ?? []) {
    const r = node.requestedReviewer;
    if (!r) continue;
    if (r.__typename === "User") out.push(r.login);
    else if (r.__typename === "Team") out.push(r.combinedSlug);
  }
  return out;
}

function extractSuggestedReviewers(
  sr: GqlPrNode["suggestedReviewers"],
): import("../types.ts").SuggestedReviewer[] {
  const out: import("../types.ts").SuggestedReviewer[] = [];
  for (const s of sr ?? []) {
    if (!s.reviewer?.login) continue;
    out.push({
      login: s.reviewer.login,
      isAuthor: s.isAuthor,
      isCommenter: s.isCommenter,
    });
  }
  return out;
}

/** Most comments the details pane keeps; older ones drop off the tail. */
const COMMENT_LIMIT = 10;

/**
 * A human authored this — has a login, isn't a GraphQL `Bot`, and isn't
 * CodeRabbit (which posts as a user-shaped app on some installs, so the
 * `Bot` check alone can miss it). CR has its own badge + thread count and
 * would otherwise drown the human conversation.
 */
function isHumanAuthor(
  a: GqlCommentAuthor,
): a is { login: string; __typename?: string } {
  if (!a?.login) return false;
  if (a.__typename === "Bot") return false;
  if (a.login === CR_LOGIN) return false;
  return true;
}

/**
 * The PR's human conversation: issue comments + non-empty review bodies,
 * merged and sorted newest-first, bots excluded, capped at
 * `COMMENT_LIMIT`. Inline review-thread comments are deliberately left
 * out — they're summarized by `countUnresolvedHumanThreads` instead of
 * inlined, which would flood the pane. ISO timestamps sort
 * lexicographically, so a string compare orders them chronologically.
 */
function extractComments(pr: GqlPrNode): PrComment[] {
  const out: PrComment[] = [];
  for (const c of pr.comments?.nodes ?? []) {
    const body = c?.body?.trim();
    if (!body || !isHumanAuthor(c.author)) continue;
    out.push({ author: c.author.login, body, createdAt: c.createdAt });
  }
  for (const r of pr.reviews?.nodes ?? []) {
    const body = r?.body?.trim();
    if (!body || !isHumanAuthor(r.author)) continue;
    out.push({ author: r.author.login, body, createdAt: r.createdAt });
  }
  out.sort((a, b) =>
    a.createdAt < b.createdAt ? 1 : a.createdAt > b.createdAt ? -1 : 0,
  );
  return out.slice(0, COMMENT_LIMIT);
}

/**
 * Count of unresolved review threads opened by a human. Reuses the
 * thread nodes already fetched for the CR rollup; a thread's opener is
 * its first comment's author.
 */
function countUnresolvedHumanThreads(
  threads: GqlReviewThread[] | null | undefined,
): number {
  let n = 0;
  for (const t of threads ?? []) {
    if (t.isResolved) continue;
    if (!isHumanAuthor(t.comments.nodes[0]?.author ?? null)) continue;
    n++;
  }
  return n;
}

export function nodeToPr(pr: GqlPrNode): PullRequest {
  const contexts =
    pr.commits.nodes[0]?.commit?.statusCheckRollup?.contexts?.nodes ?? null;
  const threads = pr.reviewThreads?.nodes ?? null;
  const requestedReviewers = extractRequestedReviewers(pr.reviewRequests);
  return {
    number: pr.number,
    url: pr.url,
    title: pr.title,
    headRefName: pr.headRefName,
    headRefOid: pr.headRefOid ?? undefined,
    baseRefName: pr.baseRefName,
    isDraft: pr.isDraft,
    state: pr.state,
    checks: openPrChecks(pr.state, rollupChecks(contexts)),
    failedChecks: pr.state === "OPEN" ? failingCheckNames(contexts) : [],
    review: rollupReview(
      pr.state,
      pr.reviewDecision,
      pr.reviewRequests?.totalCount ?? 0,
      hasStaleChangesRequest(pr.reviews, requestedReviewers),
    ),
    reviewRequests: pr.reviewRequests?.totalCount ?? 0,
    rabbit: rollupRabbit(pr.state, contexts, threads),
    requestedReviewers,
    suggestedReviewers: extractSuggestedReviewers(pr.suggestedReviewers),
    autoMerge: pr.autoMergeRequest
      ? {
          enabledAt: pr.autoMergeRequest.enabledAt,
          mergeMethod: pr.autoMergeRequest.mergeMethod,
        }
      : null,
    comments: extractComments(pr),
    unresolvedThreads: countUnresolvedHumanThreads(threads),
    mergedAt: pr.mergedAt ?? null,
    closedAt: pr.closedAt ?? null,
  };
}
