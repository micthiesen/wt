import { Effect } from "effect";

import { config } from "../config.ts";
import { createLogger } from "../logger.ts";
import { run, runStreaming } from "../proc.ts";
import type { AutoMergeMethod } from "../types.ts";
import { hasGh, repoSlug } from "./gh-cli.ts";
import type { GhActionResult, LivePrInfo } from "./types.ts";

const log = createLogger("[gh]");

const parseJsonOrNull = <A>(text: string): Effect.Effect<A | null> =>
  Effect.try(() => JSON.parse(text) as A).pipe(
    Effect.catch(() => Effect.succeed(null)),
  );

/**
 * The merge method `enableAutoMerge` arms. Hardcoded to match the repo's
 * merge style; promote to config when there's a second concrete
 * preference. Exported so the TUI's optimistic patch shows the same
 * method the gh call will actually use — the two must never drift.
 */
export const AUTO_MERGE_METHOD: AutoMergeMethod = "REBASE";

/**
 * Shared body for every one-shot `gh` write below: gh-availability
 * check, run from the main clone with the standard timeout, exit-code
 * gate, and a uniform error-logged/GhActionResult shape. Callers supply
 * only the argv and their own log label/context.
 */
const WORKFLOW_SCOPE_REMEDY =
  "This is the LOCAL gh token, not the PR: run `gh auth refresh -h github.com -s workflow` " +
  "(then `gh auth status` to confirm), and try again. Retrying as-is never clears it.";

/**
 * Did GitHub refuse this because the caller's credential lacks the
 * `workflow` scope?
 *
 * It reads as a statement about the pull request — "Pull request
 * refusing to allow an OAuth App to create or update workflow
 * `.github/workflows/ci.yml`" — and it is a statement about the token on
 * this machine. Nothing in it names a remedy, and the noun it leads with
 * sends the reader to the PR, the repo, or the merge queue.
 *
 * It bites hardest through the merge QUEUE, where nothing the user did
 * mentions a workflow file: enqueueing has GitHub create a
 * `gh-readonly-queue/...` branch carrying the PR's changes, so a PR that
 * touches `.github/workflows/` needs the scope to be enqueued at all.
 * Wording varies by credential kind (OAuth App / GitHub App / Personal
 * Access Token), so the match is on the stable half.
 *
 * Never retryable: a scope does not arrive on its own.
 */
export function missingWorkflowScope(error: string | undefined): boolean {
  if (!error) return false;
  return /refusing to allow \S+(?: \S+)* to create or update workflow/i.test(error)
    || /without\s+`?workflow`?\s+scope/i.test(error);
}

const runGhMutation = Effect.fnUntraced(function* (
  argv: string[],
  logLabel: string,
  logCtx: Record<string, unknown>,
): Effect.fn.Return<GhActionResult> {
  if (!(yield* hasGh())) return { ok: false, error: "gh CLI not found" };
  const r = yield* run(argv, {
    cwd: config.paths.mainClone,
    timeoutMs: 15_000,
  }).pipe(Effect.catch((error) => Effect.succeed({
    stdout: "",
    stderr: error.message,
    exitCode: -1,
  })));
  if (r.exitCode !== 0) {
    const raw = (r.stderr || r.stdout).trim() || `gh exited ${r.exitCode}`;
    const msg = missingWorkflowScope(raw) ? `${raw} ${WORKFLOW_SCOPE_REMEDY}` : raw;
    log.error(logLabel, { ...logCtx, msg });
    return { ok: false, error: msg };
  }
  return { ok: true };
});

/**
 * Does `branch` have a merge queue? `null` when it doesn't, when the
 * repo can't be resolved, or when the probe fails — every one of those
 * means "fall back to classic auto-merge", which is the safe direction:
 * the worst case is the error the caller would have got anyway.
 *
 * Asked per action rather than read off row state on purpose. A queue
 * is configured in branch rules and can appear or disappear between
 * polls, and this is a keystroke-driven write — one cheap round trip
 * buys a deterministic answer instead of arming the wrong feature off a
 * stale badge.
 */
const mergeQueueIdForBranch = Effect.fnUntraced(function* (
  branch: string,
): Effect.fn.Return<string | null> {
  if (!branch) return null;
  const slug = yield* repoSlug();
  if (!slug) return null;
  const [owner, name] = slug.split("/");
  if (!owner || !name) return null;
  const r = yield* run(
    [
      "gh", "api", "graphql",
      "-f",
      "query=query($owner: String!, $name: String!, $branch: String!) { repository(owner: $owner, name: $name) { mergeQueue(branch: $branch) { id } } }",
      "-f", `owner=${owner}`,
      "-f", `name=${name}`,
      "-f", `branch=${branch}`,
    ],
    { cwd: config.paths.mainClone, timeoutMs: 15_000 },
  ).pipe(Effect.catch(() => Effect.succeed(null)));
  if (r === null) return null;
  if (r.exitCode !== 0) {
    log.warn("merge-queue probe failed", { branch, msg: (r.stderr || r.stdout).slice(0, 200) });
    return null;
  }
  const parsed = yield* parseJsonOrNull<{
    data?: { repository?: { mergeQueue?: { id?: string } | null } };
  }>(r.stdout);
  return parsed?.data?.repository?.mergeQueue?.id ?? null;
});

/**
 * Enable "merge when ready" on a PR — ARM ONLY, never merge now.
 *
 * **Two different GitHub features wear that label, and picking the wrong
 * one fails outright.** A branch with a MERGE QUEUE gets
 * `enqueuePullRequest`; everything else gets classic auto-merge via
 * `enablePullRequestAutoMerge`. They are not interchangeable and they
 * are gated on different settings: classic auto-merge requires the
 * repo-level "Allow auto-merge", while a queue does not. Assuming the
 * classic mutation covered both is what produced `Auto merge is not
 * allowed for this repository` on a repo whose GitHub UI happily
 * offered the "Merge when ready" button — the button was enqueuing.
 *
 * Note the second trap the queue path steps over. Classic auto-merge
 * REFUSES on a PR with a clean status (nothing left to wait for), so on
 * a queue repo the old code would have failed on green PRs even with
 * the repo setting enabled. Enqueuing a green PR is the entire point of
 * a queue.
 *
 * `gh pr merge --auto` stays banned for the classic path: it looks
 * equivalent but silently falls through to an IMMEDIATE merge on an
 * unprotected repo — the picker's confirm-less "arm" keystroke once
 * shipped a dogfood PR on the spot while toasting "auto-merge enabled".
 */
export const enableAutoMerge = Effect.fn("enableAutoMerge")(function* (
  prId: string,
  opts: { baseRefName?: string; headRefOid?: string } = {},
): Effect.fn.Return<GhActionResult> {
  // Callers guard, but a raw GraphQL "Could not resolve to a node with
  // the global id of ''" toast is useless — fail with a named reason.
  if (!prId) return { ok: false, error: "missing PR node id" };

  const classic = (): Effect.Effect<GhActionResult> =>
    runGhMutation(
      [
        "gh", "api", "graphql",
        "-f",
        "query=mutation($prId: ID!, $method: PullRequestMergeMethod!) { enablePullRequestAutoMerge(input: {pullRequestId: $prId, mergeMethod: $method}) { pullRequest { number } } }",
        "-f", `prId=${prId}`,
        "-f", `method=${AUTO_MERGE_METHOD}`,
      ],
      "auto-merge failed",
      { prId },
    );

  const queueId = opts.baseRefName
    ? yield* mergeQueueIdForBranch(opts.baseRefName)
    : null;
  if (!queueId) return yield* classic();

  // `expectedHeadOid` makes this fail rather than enqueue a commit we
  // never saw, if the branch moved between the poll and the keystroke.
  // The merge queue's own configuration decides the merge method, so
  // AUTO_MERGE_METHOD deliberately plays no part here.
  const argv = [
    "gh", "api", "graphql",
    "-f",
    "query=mutation($prId: ID!, $oid: GitObjectID) { enqueuePullRequest(input: {pullRequestId: $prId, expectedHeadOid: $oid}) { mergeQueueEntry { position } } }",
    "-f", `prId=${prId}`,
  ];
  if (opts.headRefOid) argv.push("-f", `oid=${opts.headRefOid}`);
  const enqueued = yield* runGhMutation(argv, "enqueue failed", {
    prId,
    base: opts.baseRefName,
  });
  if (enqueued.ok) return enqueued;
  // A required check that has not REPORTED yet — never created, or
  // created and still running — is a clock, not a verdict, so it is
  // retryable where the other refusals are not.
  const pending = checksStillPending(enqueued.error);
  if (!pending && !notYetEnqueueable(enqueued.error)) return enqueued;

  // "Arm it and merge it when the checks pass" is the whole point of
  // the keystroke, and classic auto-merge is that arming: on a queue
  // base GitHub enqueues the PR itself once the requirements are met.
  // It is worth trying even while checks are pending, because on a repo
  // that allows auto-merge it turns a wait into a done thing.
  //
  // Second, not first, so the ready case still gets a queue POSITION
  // back rather than an armed flag. And the enqueue error is kept when
  // the fallback also fails: "Auto merge is not allowed for this
  // repository" alone sends the reader to a repo setting that is not
  // why this failed.
  const armed = yield* classic();
  if (armed.ok) return armed;
  return {
    ok: false,
    retryable: pending,
    error: pending
      ? `${enqueued.error} That check has not reported for this commit yet, so the refusal clears itself once CI does. (arming instead also failed: ${armed.error})`
      : `${enqueued.error} (arming instead also failed: ${armed.error})`,
  };
});

/**
 * Did the merge queue refuse this PR for a reason a CLOCK will fix?
 *
 * Two wordings, one situation, and they arrive minutes apart on the
 * same PR: `Required status check "X" is expected.` (the context has
 * reported nothing at all — a race with the workflow registering its
 * own check runs, 62 seconds on the PR that exposed it) and
 * `Required status check "X" is in progress.` (it registered and is
 * still running — 51s to 5min on this repo's suite). Neither needs a
 * human and neither survives CI finishing.
 *
 * An earlier version of this matched only `is expected`, on the belief
 * that a queue accepts a PR whose required checks are merely RUNNING.
 * That belief came from a probe carrying a deliberately-wrong
 * `expectedHeadOid`, which GitHub validates FIRST — so the probe never
 * reached check validation, and a green-looking result vouched for a
 * claim it had not tested. `is in progress` on PR #1424 refuted it
 * directly. An instrument that answers about a world it was not in
 * does not merely fail to catch the bug; it certifies it.
 *
 * Getting this wrong is expensive in a specific way: lumped in with
 * the durable refusals, a transient wait drives a fallback to classic
 * auto-merge on a repo with `allow_auto_merge: false`, and the
 * reported error names that repo setting — sending the reader to a
 * setting that had nothing to do with the failure, on a repo where
 * "merge when ready" demonstrably works from the web UI.
 *
 * Positive list, so it fails CLOSED: `has failed` and `was cancelled`
 * need someone to DO something, and a retry loop would hide that behind
 * a spinner.
 *
 * The AGGREGATE wording is a third shape and used to be lumped in with
 * the durable refusals, on the reasoning that it "mixes pending with
 * failed, so it cannot say whether waiting helps". It says exactly that,
 * in a breakdown GitHub enumerates after the colon: `4 of 4 required
 * status checks have not succeeded: 2 expected.` is four checks of which
 * two have not reported, and nothing there has failed. Refusing the whole
 * class cost the same wrong report the single-check wording did — a
 * fallback to classic auto-merge on a repo with `allow_auto_merge: false`,
 * blaming a repo setting for a clock. So the breakdown is parsed, with
 * the same discipline one level down: every reason must be a KNOWN
 * pending one, and an unrecognised token (or no breakdown at all) is not
 * a green light.
 */
export function checksStillPending(error: string | undefined): boolean {
  const msg = error ?? "";
  if (SINGLE_CHECK_PENDING_RE.test(msg)) return true;
  return aggregateIsAllPending(msg);
}

const SINGLE_CHECK_PENDING_RE =
  /required status check\b[^]*?\bis (?:expected|in progress|queued|pending|waiting)/i;

/**
 * The states in an aggregate breakdown that a CLOCK clears. Anything
 * outside this set — `failing`, `cancelled`, `action required`, or a
 * word GitHub adds next year — means the refusal is not retryable.
 */
const PENDING_CHECK_STATES = new Set([
  "expected",
  "pending",
  "queued",
  "waiting",
  "in progress",
  "in_progress",
]);

/** Everything after the colon in the aggregate refusal, up to its period. */
const AGGREGATE_RE = /required status checks?\s+have not succeeded:\s*([^.]*)/i;

function aggregateIsAllPending(error: string): boolean {
  const m = AGGREGATE_RE.exec(error);
  if (!m) return false;
  const reasons = (m[1] ?? "")
    .split(/,|\band\b/)
    .map((s) => s.trim().replace(/^\d+\s+/, "").toLowerCase())
    .filter((s) => s.length > 0);
  // A breakdown GitHub did not give is not a breakdown saying "all
  // pending". Absence of a reason means unknown, never fine.
  if (reasons.length === 0) return false;
  return reasons.every((r) => PENDING_CHECK_STATES.has(r));
}

/**
 * Did the merge queue refuse this PR because it is not READY yet, as
 * opposed to something the caller could not fix by waiting?
 *
 * Matched on GitHub's wording because the API gives nothing else: every
 * `enqueuePullRequest` refusal comes back as type `UNPROCESSABLE` with
 * a prose message, so an unready PR ("2 of 4 required status checks
 * have not succeeded: 1 expected") and a genuinely broken call ("expected
 * head oid does not match") are the same error code. Fails CLOSED — an
 * unrecognised message reports the enqueue error as-is rather than
 * silently arming a different feature, because the fallback changes
 * which GitHub feature the keystroke drove and that should never happen
 * on a guess.
 */
export function notYetEnqueueable(error: string | undefined): boolean {
  const msg = (error ?? "").toLowerCase();
  return (
    msg.includes("status checks have not succeeded") ||
    msg.includes("required status check") ||
    msg.includes("not in a mergeable state") ||
    msg.includes("is not mergeable")
  );
}

// Dequeue uses `id`; enqueue's input instead calls this `pullRequestId`.
export const DEQUEUE_PULL_REQUEST_MUTATION =
  "mutation($prId: ID!) { dequeuePullRequest(input: {id: $prId}) { mergeQueueEntry { position } } }";

/**
 * Cancel a previously-armed "merge when ready".
 *
 * Mirrors `enableAutoMerge`'s split, and has to: `--disable-auto` does
 * not remove a PR from a merge queue, so on a queue branch the cancel
 * would report success while the PR stayed queued and merged anyway.
 * Queued PRs go through `dequeuePullRequest`; everything else through
 * `gh pr merge --disable-auto`, which no-ops with an error we surface
 * verbatim when the PR wasn't armed.
 */
export const disableAutoMerge = Effect.fn("disableAutoMerge")(function* (
  prNumber: number,
  opts: { prId?: string; baseRefName?: string } = {},
): Effect.fn.Return<GhActionResult> {
  const queueId =
    opts.prId && opts.baseRefName
      ? yield* mergeQueueIdForBranch(opts.baseRefName)
      : null;
  if (queueId && opts.prId) {
    return yield* runGhMutation(
      [
        "gh", "api", "graphql",
        "-f",
        `query=${DEQUEUE_PULL_REQUEST_MUTATION}`,
        "-f", `prId=${opts.prId}`,
      ],
      "dequeue failed",
      { prNumber, base: opts.baseRefName },
    );
  }
  return yield* runGhMutation(
    ["gh", "pr", "merge", String(prNumber), "--disable-auto"],
    "disable auto-merge failed",
    { prNumber },
  );
});

/**
 * Edit a PR's review requests via `gh pr edit`. Both `add` and
 * `remove` may be passed in the same call — gh accepts both flag
 * sets at once. Logins are users; team slugs use the `org/team-slug`
 * form. Empty changes is a no-op.
 */
export function editReviewers(
  prNumber: number,
  changes: { add: readonly string[]; remove: readonly string[] },
): Effect.Effect<GhActionResult> {
  if (changes.add.length === 0 && changes.remove.length === 0) {
    return Effect.succeed({ ok: true });
  }
  const argv = ["gh", "pr", "edit", String(prNumber)];
  for (const l of changes.add) argv.push("--add-reviewer", l);
  for (const l of changes.remove) argv.push("--remove-reviewer", l);
  return runGhMutation(argv, "edit reviewers failed", { prNumber, changes });
}

/**
 * Retarget a PR's base branch via `gh pr edit --base`. The native restack
 * engine calls this after replaying a branch whose parent moved (e.g. a
 * child reparented onto trunk once its parent landed), so the PR's base on
 * GitHub matches the recorded parent. No-op-safe: gh is idempotent if the base
 * already matches. Runs from the main clone so gh resolves the right repo.
 */
export function retargetPrBase(
  prNumber: number,
  base: string,
): Effect.Effect<GhActionResult> {
  return runGhMutation(
    ["gh", "pr", "edit", String(prNumber), "--base", base],
    "retarget pr base failed",
    { prNumber, base },
  );
}

/**
 * Flip a draft PR to "ready for review" via `gh pr ready`. Notifies
 * reviewers and triggers any code-owner auto-requests, so callers
 * should gate on user confirmation. Runs from the main clone so gh
 * resolves the right repo.
 */
export function markPullRequestReady(
  prNumber: number,
): Effect.Effect<GhActionResult> {
  return runGhMutation(
    ["gh", "pr", "ready", String(prNumber)],
    "mark ready failed",
    { prNumber },
  );
}

/**
 * Close a GitHub issue on the origin repo as completed. Used by the
 * `builtin:close-issue` automation once a worktree's branch lands.
 * Callers treat failure as advisory — the issue may already be closed
 * (a PR-body closing keyword, or a hand close, beat us to it) — so
 * they log and move on rather than surfacing an error.
 */
export function closeGithubIssue(issue: number): Effect.Effect<GhActionResult> {
  return runGhMutation(
    ["gh", "issue", "close", String(issue), "--reason", "completed"],
    "close issue failed",
    { issue },
  );
}

/**
 * Delete a landed branch's ref on the origin repo. Used by the
 * `builtin:delete-branch` automation once a worktree's branch merges —
 * the same effect as GitHub's "Automatically delete head branches"
 * setting, for repos that have not enabled it.
 *
 * The trunk refusal is defence in depth rather than a live worry: the
 * only branch that ever reaches here is a worktree's own, frozen onto a
 * fire that already required the branch to have LANDED. It is here
 * because the blast radius is asymmetric — every other failure in this
 * file costs a retry, and this one costs the repo's mainline.
 *
 * Failure is advisory, exactly like `closeGithubIssue`: a repo with the
 * GitHub setting on, or anyone who deleted it by hand, gets there first
 * and GitHub answers `Reference does not exist`. That is the desired
 * end state, not an error, so callers log and move on rather than
 * retrying into a ref that is already gone.
 */
export const deleteRemoteBranch = Effect.fn("deleteRemoteBranch")(function* (
  branch: string,
): Effect.fn.Return<GhActionResult> {
  if (!branch) return { ok: false, error: "missing branch" };
  if (branch === config.branch.base) {
    return { ok: false, error: `refusing to delete the trunk branch ${branch}` };
  }
  const slug = yield* repoSlug();
  if (!slug) return { ok: false, error: "could not resolve the origin repo" };
  // Nested refs (`user/feature`) need no escaping — the REST path takes
  // the rest of the ref verbatim after `heads/`.
  return yield* runGhMutation(
    ["gh", "api", "--method", "DELETE", `repos/${slug}/git/refs/heads/${branch}`],
    "delete branch failed",
    { branch },
  );
});

/**
 * Stream the failed-job logs of the most recent failed CI run for
 * `branch` to `onLine`, via `gh run view <id> --log-failed`. Resolves
 * the count of lines emitted, or a reason when gh is missing, no failed
 * run exists (a check can fail as a bare `StatusContext` with no Actions
 * run behind it), or gh errors. Read-only; safe to fire from a keybind.
 */
export const streamFailedRunLog = Effect.fn("streamFailedRunLog")(function* (
  branch: string,
  onLine: (line: string) => void,
): Effect.fn.Return<{ ok: true } | { ok: false; reason: string }> {
  if (!(yield* hasGh())) return { ok: false, reason: "gh CLI not found" };
  const listed = yield* run(
    [
      "gh", "run", "list",
      "--branch", branch,
      "--status", "failure",
      "--limit", "1",
      "--json", "databaseId",
    ],
    { cwd: config.paths.mainClone, timeoutMs: 15_000 },
  ).pipe(Effect.catch((error) => Effect.succeed({
    stdout: "",
    stderr: error.message,
    exitCode: -1,
  })));
  if (listed.exitCode !== 0) {
    return { ok: false, reason: (listed.stderr || listed.stdout).trim() || "gh run list failed" };
  }
  const runs = yield* parseJsonOrNull<Array<{ databaseId?: number }>>(
    listed.stdout,
  );
  if (!runs) return { ok: false, reason: "could not parse gh run list" };
  const runId = runs[0]?.databaseId;
  if (runId === undefined) return { ok: false, reason: "no failed workflow run" };
  const code = yield* runStreaming(
    ["gh", "run", "view", String(runId), "--log-failed"],
    { cwd: config.paths.mainClone, onLine },
  ).pipe(Effect.catch(() => Effect.succeed(-1)));
  if (code !== 0) return { ok: false, reason: `gh run view exited ${code}` };
  return { ok: true };
});

/**
 * Read the live `baseRefName` / `state` for a branch's PR via
 * `gh pr view`. The restack reconcile/retarget paths use it to compare
 * the recorded parent against the PR's actual base. Returns null when
 * there's no PR (or gh is unavailable).
 */
export const viewPrInfo = Effect.fn("viewPrInfo")(function* (
  branch: string,
): Effect.fn.Return<LivePrInfo | null> {
  if (!branch) return null;
  if (!(yield* hasGh())) return null;
  const r = yield* run(
    [
      "gh", "pr", "view", branch,
      "--json", "number,baseRefName,state,isDraft,title,id,headRefOid",
    ],
    { cwd: config.paths.mainClone, timeoutMs: 15_000 },
  ).pipe(Effect.catch(() => Effect.succeed(null)));
  if (r === null) return null;
  if (r.exitCode !== 0) return null;
  const d = yield* parseJsonOrNull<Partial<LivePrInfo>>(r.stdout);
  if (!d || typeof d.number !== "number") return null;
  // Validate `state` against the known set rather than asserting — gh
  // could in principle return a value outside the union, and downstream
  // merge-detection branches on it.
  const state: LivePrInfo["state"] =
    d.state === "CLOSED" || d.state === "MERGED" ? d.state : "OPEN";
  return {
    number: d.number,
    baseRefName: typeof d.baseRefName === "string" ? d.baseRefName : "",
    state,
    isDraft: d.isDraft === true,
    title: typeof d.title === "string" ? d.title : "",
    id: typeof d.id === "string" ? d.id : "",
    headRefOid: typeof d.headRefOid === "string" ? d.headRefOid : "",
  };
});
