import { Data, Effect } from "effect";

import { config } from "../config.ts";
import { causeMessage } from "../errors.ts";
import { createLogger } from "../logger.ts";
import { run } from "../proc.ts";
import { GH_TIMEOUT_MS, ghFailureMessage, hasGh } from "./gh-cli.ts";
import { openPrChecks, rollupChecks } from "./parse.ts";
import type { RawCheck } from "./types.ts";
import type { ReviewRequestPr } from "./types.ts";

const log = createLogger("[gh]");

export class ReviewRequestsError extends Data.TaggedError("ReviewRequestsError")<{
  readonly cause: unknown;
}> {
  override get message(): string {
    return causeMessage(this.cause);
  }
}

/**
 * Pull requests where the authenticated user (or one of their teams)
 * has been asked to review. Uses GitHub's `search` GraphQL — same auth
 * channel as `fetchGithub`, but a separate round trip because the
 * result set isn't keyed by worktree branches and the response shape is
 * narrower (no review threads, no requested reviewers list, no
 * suggestedReviewers). Capped at 50 since this is meant to be a
 * digestible "what's on your plate" list, not an inbox.
 */
const REVIEW_REQUESTS_QUERY = `
query {
  search(query: "is:pr is:open review-requested:@me", type: ISSUE, first: 50) {
    nodes {
      ... on PullRequest {
        number
        url
        title
        isDraft
        createdAt
        updatedAt
        author { login }
        repository { nameWithOwner }
        headRefName
        additions
        deletions
        changedFiles
        reviewDecision
        comments { totalCount }
        commits(last: 1) {
          nodes {
            commit {
              statusCheckRollup {
                contexts(first: 50) {
                  nodes {
                    __typename
                    ... on CheckRun {
                      name
                      status
                      conclusion
                      startedAt
                      checkSuite { workflowRun { databaseId workflow { databaseId } } }
                    }
                    ... on StatusContext { context state createdAt }
                  }
                }
              }
            }
          }
        }
      }
    }
  }
}`;

type GqlReviewRequestNode = {
  number?: number;
  url?: string;
  title?: string;
  isDraft?: boolean;
  createdAt?: string;
  updatedAt?: string;
  author?: { login?: string | null } | null;
  repository?: { nameWithOwner?: string } | null;
  headRefName?: string | null;
  additions?: number;
  deletions?: number;
  changedFiles?: number;
  reviewDecision?: string | null;
  comments?: { totalCount?: number } | null;
  commits?: {
    nodes: Array<{
      commit: {
        statusCheckRollup: { contexts: { nodes: RawCheck[] } } | null;
      };
    }>;
  } | null;
};

type GqlReviewRequestResponse = {
  // `search(type: ISSUE)` returns a heterogeneous node list; for a node
  // that resolved to a non-`PullRequest` typename (or an empty object
  // from a deleted/inaccessible item) the spread fragment yields `{}`.
  // Modelled as nullable so the parser doesn't crash on those.
  data?: { search?: { nodes?: Array<GqlReviewRequestNode | null> } };
};

/** Exact, case-insensitive match against GitHub's canonical `owner/repo` name. */
export function reviewRepositoryIsIgnored(
  repository: string | null | undefined,
  ignored: readonly string[] = config.github.ignoredReviewRepositories,
): boolean {
  if (!repository) return false;
  const target = repository.trim().toLowerCase();
  return ignored.some((name) => name.trim().toLowerCase() === target);
}

export const fetchReviewRequests = Effect.fn("fetchReviewRequests")(function* (
  signal?: AbortSignal,
): Effect.fn.Return<ReviewRequestPr[], ReviewRequestsError> {
  if (!(yield* hasGh())) return [];
  const r = yield* run(
    ["gh", "api", "graphql", "-f", `query=${REVIEW_REQUESTS_QUERY}`],
    { cwd: config.paths.mainClone, timeoutMs: GH_TIMEOUT_MS, signal },
  ).pipe(
    Effect.catch((cause) =>
      signal?.aborted
        ? Effect.succeed(null)
        : Effect.fail(new ReviewRequestsError({ cause })),
    ),
  );
  if (r === null) return [];
  if (r.exitCode !== 0) {
    // An aborted signal means the query was cancelled mid-flight (refs
    // churn invalidates this query; `run` SIGTERMs the child → exit
    // 143). TanStack discards the cancelled fetch's result, so this is
    // routine supersession, not a failure — stay silent.
    if (signal?.aborted) return [];
    // `gh api graphql` puts GraphQL errors / rate-limit bodies on
    // stdout, not stderr — and a timeout/abort leaves both empty with
    // only a non-zero exit code. Log all three so the failure is
    // actually diagnosable instead of `{"stderr":""}`.
    log.error("review-requests fetch failed", {
      exitCode: r.exitCode,
      timedOut: r.timedOut ?? false,
      stderr: r.stderr.slice(0, 200) || null,
      stdout: r.stdout.slice(0, 200) || null,
    });
    // Throw rather than return [] — an empty success would blank the
    // review-requests section on a transient blip; a rejection keeps
    // the last good list and marks the query errored.
    return yield* new ReviewRequestsError({
      cause: new Error(
        `review-requests fetch failed: ${ghFailureMessage(r, true)}`,
      ),
    });
  }
  const parsed = yield* Effect.try({
    try: () => JSON.parse(r.stdout) as GqlReviewRequestResponse,
    catch: (cause) => new ReviewRequestsError({ cause }),
  }).pipe(
    Effect.tapError((error) => Effect.sync(() => {
      log.error(
        error.cause instanceof Error ? error.cause : String(error.cause),
        { stdout: r.stdout.slice(0, 200) },
      );
    })),
    Effect.mapError(() => new ReviewRequestsError({
      cause: new Error("review-requests fetch failed: unparseable gh output"),
    })),
  );
  const nodes = parsed.data?.search?.nodes ?? [];
  const out: ReviewRequestPr[] = [];
  for (const n of nodes) {
    // Drop incomplete nodes defensively — search returns `... on
    // PullRequest`-typed fragments, so a null or empty object means it
    // wasn't a PR (shouldn't happen with `is:pr` filter, but the type
    // is `[Issue | PullRequest | ...]`).
    if (!n) continue;
    if (typeof n.number !== "number" || !n.url || !n.title) continue;
    const repoNameWithOwner = n.repository?.nameWithOwner ?? "";
    if (reviewRepositoryIsIgnored(repoNameWithOwner)) continue;
    const contexts =
      n.commits?.nodes[0]?.commit?.statusCheckRollup?.contexts?.nodes ?? null;
    const decision =
      n.reviewDecision === "APPROVED" ||
      n.reviewDecision === "CHANGES_REQUESTED" ||
      n.reviewDecision === "REVIEW_REQUIRED"
        ? n.reviewDecision
        : null;
    out.push({
      number: n.number,
      url: n.url,
      title: n.title,
      repoNameWithOwner,
      headRefName: n.headRefName ?? null,
      author: n.author?.login ?? null,
      isDraft: n.isDraft ?? false,
      // The search is `is:open`, so every row here is an open PR — floor an
      // empty rollup at `pending` the same way the worktree rows do.
      checks: openPrChecks("OPEN", rollupChecks(contexts)),
      reviewDecision: decision,
      additions: n.additions ?? 0,
      deletions: n.deletions ?? 0,
      changedFiles: n.changedFiles ?? 0,
      commentCount: n.comments?.totalCount ?? 0,
      createdAt: n.createdAt ?? "",
      updatedAt: n.updatedAt ?? "",
    });
  }
  return out;
});
