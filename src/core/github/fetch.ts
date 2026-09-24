import { Data, Effect, Schedule } from "effect";

import { config } from "../config.ts";
import { createLogger } from "../logger.ts";
import { run, type RunResult } from "../proc.ts";
import type { MergeQueueEntry, PullRequest } from "../types.ts";
import { listWorktrees, type WorktreeError } from "../worktree.ts";
import { GH_TIMEOUT_MS, ghFailureMessage, hasGh, repoSlug } from "./gh-cli.ts";
import { nodeToPr } from "./parse.ts";
import type { GithubData, GqlResponse } from "./types.ts";

const log = createLogger("[gh]");

export class GithubTransportError extends Data.TaggedError(
  "GithubTransportError",
)<{ readonly message: string; readonly cause?: unknown }> {}

export class GithubProtocolError extends Data.TaggedError(
  "GithubProtocolError",
)<{ readonly message: string; readonly cause?: unknown }> {}

export class GithubRateLimitError extends Data.TaggedError(
  "GithubRateLimitError",
)<{ readonly message: string }> {}

export class GithubTransientError extends Data.TaggedError(
  "GithubTransientError",
)<{ readonly message: string; readonly result: RunResult }> {}

export type GithubFetchError =
  | GithubTransportError
  | GithubProtocolError
  | GithubRateLimitError
  | GithubTransientError;

/** First non-empty line, trimmed — gh failure bodies are multiline. */
function firstLine(s: string): string {
  return (
    s
      .split("\n")
      .find((l) => l.trim().length > 0)
      ?.trim() ?? ""
  );
}

// Invariant: the github source is ONE batched fetch. Every per-worktree
// PR field (state, checks, reviews, requested + suggested reviewers, ...)
// rides the same aliased query below, alongside the repo mergeQueue
// block. New PR fields go into PR_FRAGMENT here — never a new query;
// rate limits and latency are real.
//
// "One batch" is about not fetching PER ROW. It is NOT "one HTTP
// request": the batch is split into a few fixed-size chunks below, for
// a ceiling that has nothing to do with rate limits. See CHUNK_SIZE.

// Comment window. 10 covers the details-pane conversation; checklist
// review-bot mode ALSO finds the bot's summary comment in this window,
// so it widens — a summary buried past the window would silently read
// as "the bot never ran".
const COMMENT_FETCH_LIMIT =
  config.reviewBot.unresolvedVia === "checklist" ? 30 : 10;

/**
 * Branches per GraphQL round trip.
 *
 * GitHub gives a GraphQL query roughly ten seconds of server-side
 * execution, and this query's cost is linear in branch count at about
 * 250ms per branch. Measured against a real repo: 24 branches took
 * 6.9-7.7s in a single request, so a fleet at 35 sat on the ceiling.
 * That is a LATENCY CLIFF, not a quota, and the distinction is the
 * whole reason this constant exists — on the day it was diagnosed, 126
 * of 130 failures were at exactly `branchCount: 35`, every one of them
 * a 502/504, GitHub's own "couldn't respond to your request in time",
 * an HTTP/2 stream CANCEL or a truncated body, while 2184 of 5000
 * GraphQL points sat unspent. Nothing was rate limited.
 *
 * Note which fixes that rules out. Throttling our side is the natural
 * reflex and is exactly backwards: fewer, larger, less frequent
 * requests is the direction that fails. Trimming fields does not rescue
 * it either, because no single field dominates — dropping the most
 * expensive one takes 7.5s to 6.1s, and stripping everything but
 * scalars still leaves 2.2s.
 *
 * Splitting is close to free on the budget that actually IS metered:
 * GraphQL points are computed from node count, so the same 24 branches
 * cost 27 points as one request and 27-28 spread across three or four.
 * Wall clock improves at the same time, because the chunks run
 * concurrently: 7.7s at one request, 3.2s at chunk 8, 2.4s at chunk 6.
 *
 * 8 keeps a chunk near 3s — a third of the budget — so the cliff stays
 * far away as the fleet grows. Concurrency is deliberately left to
 * `run`'s own RUN_CONCURRENCY rather than a private cap here: that is a
 * render-thread spawn budget, which is precisely the resource a wide
 * fan-out would otherwise take from the rest of the TUI.
 */
export const CHUNK_SIZE = 8;

/** Includes cross-process gate wait as well as the GitHub round trip. */
const ATTEMPT_TIMEOUT_MS = GH_TIMEOUT_MS;

/** Attempts per chunk: one try plus two retries. */
const MAX_ATTEMPTS = 3;

/** Backoff base; each retry doubles it and adds full jitter. */
const RETRY_BASE_MS = 400;

/**
 * Ceiling across every attempt of every chunk. Bounds the pathological
 * case (all chunks timing out and retrying) well inside the caller's
 * poll interval, so a wedged fetch can't still be running when the next
 * one starts.
 */
const RETRY_DEADLINE_MS = 100_000;

/**
 * Failures a retry can actually fix. A 502/504, GitHub's own "couldn't
 * respond in time", an HTTP/2 CANCEL and a truncated body are the same
 * event seen from four different layers: the query ran out of
 * server-side time.
 */
const TRANSIENT_PATTERNS = [
  /\bHTTP 5\d\d\b/,
  /couldn't respond to your request in time/i,
  /no server is currently available/i,
  /stream error:.*\bCANCEL\b/i,
  /unexpected end of JSON input/i,
  /\b(connection reset|broken pipe|i\/o timeout|unexpected EOF)\b/i,
  /\b(INTERNAL|SERVICE_UNAVAILABLE|TIMEOUT)\b/,
  /something went wrong while executing your query/i,
  /internal server error/i,
];

/**
 * Never retried, and the rate-limit entries are the load-bearing half:
 * retrying a rate limit is what turns a brush with the limit into a
 * block. Auth failures, malformed queries and 404s are simply permanent
 * — a retry spends a round trip to be told the same thing.
 */
const PERMANENT_PATTERNS = [
  /rate limit/i,
  /RATE_LIMITED/,
  /secondary rate/i,
  /abuse detection/i,
  /Bad credentials/i,
  /authentication/i,
  /Could not resolve to a Repository/i,
];

export function isTransientFailure(r: RunResult): boolean {
  const body = `${r.stderr}\n${r.stdout}`;
  if (PERMANENT_PATTERNS.some((re) => re.test(body))) return false;
  // Bun reports a SIGKILL as 137 on macOS, not necessarily a negative exit
  // code. The explicit timeout flag distinguishes our deadline from an
  // unrelated external kill.
  if (r.timedOut) return true;
  return TRANSIENT_PATTERNS.some((re) => re.test(body));
}

export function chunkBranches(branches: string[], size: number): string[][] {
  const out: string[][] = [];
  for (let i = 0; i < branches.length; i += size)
    out.push(branches.slice(i, i + size));
  return out;
}

// Shared fields for each PR. Used by every aliased sub-query below.
const PR_FRAGMENT = `
fragment PrFields on PullRequest {
  id
  number
  url
  title
  headRefName
  headRefOid
  baseRefName
  isDraft
  state
  mergeable
  mergeStateStatus
  mergedAt
  closedAt
  reviewDecision
  reviewRequests(first: 20) {
    totalCount
    nodes {
      requestedReviewer {
        __typename
        ... on User { login }
        ... on Team { combinedSlug }
      }
    }
  }
  suggestedReviewers {
    reviewer { login }
    isAuthor
    isCommenter
  }
  autoMergeRequest {
    enabledAt
    mergeMethod
  }
  commits(last: 1) {
    nodes {
      commit {
        committedDate
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
  reviewThreads(first: 50) {
    nodes {
      isResolved
      comments(first: 1) {
        nodes { author { login __typename } }
      }
    }
  }
  comments(last: ${COMMENT_FETCH_LIMIT}) {
    nodes {
      author { login __typename }
      body
      createdAt
      updatedAt
    }
  }
  reviews(last: 10) {
    nodes {
      author { login __typename }
      body
      state
      createdAt
    }
  }
}`;

/**
 * Build a graphql doc with one aliased `pullRequests(headRefName:)`
 * sub-query per branch in THIS chunk. `first: 2` per branch catches the
 * rare "branch has a reopen" case where there's an OPEN and a terminal
 * PR on the same ref — we'll prefer OPEN at parse time.
 *
 * Scoping to exact branches rather than pulling the 100 most recent
 * drops wall-clock ~4x and response size ~8x at 10 worktrees; it also
 * means the query cost is bounded by the number of worktrees, not by
 * how busy the repo is.
 *
 * The merge queue is repo-wide, so it rides exactly one chunk rather
 * than being refetched by each.
 */
export function buildQuery(
  branchCount: number,
  withMergeQueue: boolean,
): string {
  const varDecls = Array.from(
    { length: branchCount },
    (_, i) => `$b${i}: String!`,
  ).join(", ");
  const aliases = Array.from(
    { length: branchCount },
    (_, i) =>
      `    wt_${i}: pullRequests(first: 2, headRefName: $b${i}, orderBy: {field: UPDATED_AT, direction: DESC}) { nodes { ...PrFields } }`,
  ).join("\n");
  // `mergeQueue` with no `branch:` argument resolves to the DEFAULT
  // branch's queue, which is the wrong queue whenever worktrees target
  // something else. Measured on a repo whose queue lives on `staging`
  // while the default branch is `main`: the unqualified field returned
  // null on every poll, so the merge-queue position badge could never
  // render and read as "no queue configured" rather than "asked the
  // wrong branch". Worktree PRs target `branch.base` (a stacked PR
  // targets its parent, which is a feature branch and never has a
  // queue), so that is the only queue whose entries can describe a row.
  const mergeQueue = withMergeQueue
    ? `
    mergeQueue(branch: $mergeQueueBranch) {
      entries(first: 50) {
        nodes {
          enqueuedAt
          estimatedTimeToMerge
          position
          state
          pullRequest { headRefName }
        }
      }
    }`
    : "";
  const mqVar = withMergeQueue ? ", $mergeQueueBranch: String!" : "";
  return `
query($owner: String!, $name: String!${mqVar}, ${varDecls}) {
  repository(owner: $owner, name: $name) {
${aliases}${mergeQueue}
  }
}
${PR_FRAGMENT}`;
}

/**
 * One chunk's round trip, retried on transient failure. Retries live
 * here rather than at the TanStack observer for two reasons: a retry
 * re-runs only the chunk that failed instead of the whole fan-out, and
 * the CLI callers plus the webhook daemon (which never touch the query
 * client) get the same resilience.
 *
 * The TUI's global `retry: false` is deliberate and still right for
 * anything a keystroke drives — but this fetch is a background poll
 * with no user watching, and `keepPreviousData` holds the last good
 * badges on screen throughout, so a retry here is invisible rather than
 * "annoying in a TUI".
 */
type GithubExecutor = (
  args: readonly string[],
) => Effect.Effect<RunResult, GithubTransportError>;

const executeGithub: GithubExecutor = (args) =>
  run(args, {
    cwd: config.paths.mainClone,
    timeoutMs: ATTEMPT_TIMEOUT_MS,
  }).pipe(
    Effect.mapError(
      (cause) => new GithubTransportError({ message: cause.message, cause }),
    ),
  );

const classifyFailure = (result: RunResult): GithubFetchError => {
  const message = ghFailureMessage(result, true);
  const body = `${result.stderr}\n${result.stdout}`;
  if (PERMANENT_PATTERNS.some((pattern) => pattern.test(body))) {
    if (/rate limit|RATE_LIMITED|abuse detection/i.test(body)) {
      return new GithubRateLimitError({ message });
    }
    return new GithubTransportError({ message });
  }
  return isTransientFailure(result)
    ? new GithubTransientError({ message, result })
    : new GithubTransportError({ message });
};

const classifyProtocolFailure = (result: RunResult): GithubFetchError => {
  const body = `${result.stderr}\n${result.stdout}`;
  if (PERMANENT_PATTERNS.some((pattern) => pattern.test(body))) {
    return classifyFailure({ ...result, exitCode: result.exitCode || 1 });
  }
  try {
    const parsed = JSON.parse(result.stdout) as GqlResponse;
    if (parsed.errors?.length) {
      const graphqlBody = parsed.errors
        .map((error) => [error.type, error.message].filter(Boolean).join(": "))
        .join("\n");
      return classifyFailure({
        ...result,
        stdout: graphqlBody,
        exitCode: result.exitCode || 1,
      });
    }
  } catch (cause) {
    return new GithubTransientError({
      message:
        cause instanceof Error ? cause.message : "unparseable gh JSON output",
      result,
    });
  }
  return new GithubProtocolError({
    message: firstLine(result.stdout) || "missing GitHub repository payload",
  });
};

const retrySchedule = Schedule.exponential(RETRY_BASE_MS).pipe(
  Schedule.jittered,
);

export function fetchChunk(
  owner: string,
  name: string,
  branches: string[],
  withMergeQueue: boolean,
  execute: GithubExecutor = executeGithub,
): Effect.Effect<GithubData, GithubFetchError> {
  const query = buildQuery(branches.length, withMergeQueue);
  const args = [
    "gh",
    "api",
    "graphql",
    "-f",
    `query=${query}`,
    "-f",
    `owner=${owner}`,
    "-f",
    `name=${name}`,
  ];
  if (withMergeQueue) args.push("-f", `mergeQueueBranch=${config.branch.base}`);
  for (let i = 0; i < branches.length; i++) {
    args.push("-f", `b${i}=${branches[i]}`);
  }

  const attempt = Effect.suspend(() => execute(args)).pipe(
    Effect.flatMap((result) => {
      if (result.exitCode !== 0) return Effect.fail(classifyFailure(result));
      const parsed = parseChunk(result, branches, withMergeQueue);
      if (parsed) return Effect.succeed(parsed);
      return Effect.fail(classifyProtocolFailure(result));
    }),
  );
  return attempt.pipe(
    Effect.retry({
      times: MAX_ATTEMPTS - 1,
      schedule: retrySchedule,
      while: (error) => error._tag === "GithubTransientError",
    }),
  );
}

export function fetchChunks(
  owner: string,
  name: string,
  groups: string[][],
  execute: GithubExecutor = executeGithub,
): Effect.Effect<GithubData, GithubFetchError> {
  return Effect.all(
    groups.map((group, index) =>
      fetchChunk(owner, name, group, index === 0, execute),
    ),
    { concurrency: "unbounded" },
  ).pipe(
    Effect.map((chunks) => {
      const prs = new Map<string, PullRequest>();
      const mergeQueue = new Map<string, MergeQueueEntry>();
      for (const chunk of chunks) {
        for (const [key, value] of chunk.prs) prs.set(key, value);
        for (const [key, value] of chunk.mergeQueue) mergeQueue.set(key, value);
      }
      return { prs, mergeQueue };
    }),
  );
}

/**
 * Parse one chunk's response. Returns null when the body is
 * structurally unusable, which the caller treats as a retryable
 * failure rather than as data.
 */
function parseChunk(
  r: RunResult,
  branches: string[],
  withMergeQueue: boolean,
): GithubData | null {
  let parsed: GqlResponse;
  try {
    parsed = JSON.parse(r.stdout);
  } catch {
    return null;
  }
  // GraphQL may return exit 0 and useful-looking partial data alongside an
  // errors array. Accepting it would turn every omitted alias into "no PR".
  if (parsed.errors?.length) return null;
  const repo = parsed.data?.repository;
  if (!repo) return null;

  const prs = new Map<string, PullRequest>();
  for (let i = 0; i < branches.length; i++) {
    const nodes = repo[`wt_${i}`]?.nodes ?? [];
    if (nodes.length === 0) continue;
    // Prefer OPEN when a branch has multiple PRs (reopens etc.). Sort
    // is stable, so among non-OPEN we keep UPDATED_AT-desc order.
    const sorted = [...nodes].sort(
      (a, b) => (a.state === "OPEN" ? 0 : 1) - (b.state === "OPEN" ? 0 : 1),
    );
    const chosen = sorted[0];
    if (!chosen || !chosen.headRefName) continue;
    prs.set(chosen.headRefName, nodeToPr(chosen));
  }

  const mergeQueue = new Map<string, MergeQueueEntry>();
  if (withMergeQueue) {
    for (const n of repo.mergeQueue?.entries?.nodes ?? []) {
      const head = n.pullRequest?.headRefName;
      if (!head) continue;
      mergeQueue.set(head, {
        headRefName: head,
        position: n.position,
        state: n.state,
        enqueuedAt: n.enqueuedAt,
        estimatedTimeToMerge: n.estimatedTimeToMerge,
      });
    }
  }

  return { prs, mergeQueue };
}

/**
 * Fetch PRs for a fixed set of branches + merge-queue entries, as a
 * small fixed number of concurrent graphql round trips (see
 * CHUNK_SIZE). Pass the exact worktree branches; anything not on the
 * list is never fetched (the TUI wouldn't display it anyway).
 *
 * `signal` (when provided) cascades into every underlying `gh`
 * invocation so a superseded query — branch list re-keyed before the
 * previous fetch returned — actually stops the subprocesses instead of
 * letting them burn graphql round trips on data nobody will read.
 */
export const fetchGithub = Effect.fn("fetchGithub")(function* (
  branches: string[],
): Effect.fn.Return<GithubData, GithubFetchError> {
  const empty: GithubData = { prs: new Map(), mergeQueue: new Map() };
  if (!(yield* hasGh())) return empty;
  const slug = yield* repoSlug();
  if (!slug) return empty;
  const [owner, name] = slug.split("/");
  if (!owner || !name || branches.length === 0) return empty;

  const groups = chunkBranches(branches, CHUNK_SIZE);
  return yield* fetchChunks(owner, name, groups).pipe(
    Effect.timeoutOrElse({
      duration: RETRY_DEADLINE_MS,
      orElse: () =>
        Effect.fail(
          new GithubTransientError({
            message: "github fetch retry budget exhausted",
            result: { stdout: "", stderr: "", exitCode: -1, timedOut: true },
          }),
        ),
    }),
    Effect.tapError((error) =>
      Effect.sync(() =>
        log.error("gh api graphql failed", {
          totalChunks: groups.length,
          branchCount: branches.length,
          error: error.message,
        }),
      ),
    ),
  );
});

/**
 * Thin wrapper kept for CLI callers (doctor, ls) that don't have a
 * prebuilt branch list. Resolves branches from `git worktree list`
 * before fetching. Degrades to an empty map on fetch failure — a CLI
 * listing without PR columns beats a crashed listing — but says so on
 * stderr instead of impersonating "no PRs".
 */
export const fetchPrs = Effect.fn("fetchPrs")(
  function* (): Effect.fn.Return<
    Map<string, PullRequest>,
    WorktreeError | GithubFetchError
  > {
    const worktrees = yield* listWorktrees();
    const branches = worktrees
      .filter((worktree) => !worktree.isMain && worktree.branch)
      .map((worktree) => worktree.branch as string);
    return (yield* fetchGithub(branches)).prs;
  },
  Effect.catch((error) =>
    Effect.sync(() => {
      console.error(
        `wt: ${error instanceof Error ? error.message : String(error)} (PR info omitted)`,
      );
      return new Map<string, PullRequest>();
    }),
  ),
);
