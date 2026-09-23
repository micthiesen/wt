/**
 * GitHub PR mutation flows (mark ready / auto-merge / ship), extracted
 * from `app.tsx`. Same pattern as the other `flows/*` modules:
 * `makeGithubPrFlows` is called per render with the current rows +
 * helpers so the returned closures always see fresh state. All three
 * flows follow the optimistic-patch rules from `state/hooks.ts` —
 * patch `["github"]` via `patchPullRequest`, reconcile on settle.
 *
 * Each keystroke entrypoint (`doMarkReady`, `doAutoMerge`, `doShipPr`)
 * stays a thin `Effect.runPromise` wrapper around the exported Effect
 * that holds the actual logic (`markReady`, `setAutoMerge`, `shipPr`).
 *
 * `mutate`'s `run` is an `Effect`, so `runOptimisticMutation` wraps its
 * failure exactly once, tagged `"hooks"` (see `state/hooks.ts`). Every
 * `mutate(...)` call below is adopted with `io.promise(label, …)`
 * (this file's own boundary, tagged `"github pr flows"`), so a failure
 * arrives as `OperationError{ cause: OperationError{ cause: <original> } }`
 * — one level deeper than a bare `mutate()` rejection. Reading the
 * *inner* `.cause` (`causeMessage(outer.cause)`) peels exactly that one
 * extra layer back off, reproducing the same wording a direct
 * `try { await mutate(...) } catch (err) { err.message }` would have
 * shown.
 */
import { config } from "../../core/config.ts";
import {
  AUTO_MERGE_METHOD,
  disableAutoMerge,
  editReviewers,
  enableAutoMerge,
  markPullRequestReady,
  streamFailedRunLog,
} from "../../core/github.ts";
import { causeMessage, operationErrors, type OperationError } from "../../core/errors.ts";
import { createLogger } from "../../core/logger.ts";
import type { PullRequest } from "../../core/types.ts";
import { patchPullRequest, type GithubData } from "../../state/index.ts";
import type { QueryFilters } from "@tanstack/react-query";
import { Clock, Data, DateTime, Effect } from "effect";

import { armedFromPr } from "../badges.ts";
import {
  autoMergeRetryPending,
  cancelAutoMergeRetry,
  RETRY_LIMIT_MS,
  startAutoMergeRetry,
} from "./auto-merge-retry.ts";
import type { WorktreeRow } from "../hooks/useWorktreeRows.ts";
import type { ActionSubject } from "../action-subject.ts";
import { theme } from "../theme.ts";

const io = operationErrors("github pr flows");

/**
 * A `GhActionResult` coming back `{ ok: false }` inside a `mutate`
 * `run:` effect — tagged (rather than a bare `Error`) so it doesn't
 * merge into an untyped failure channel. `.message` is the raw
 * `result.error` verbatim, so wording downstream (`causeMessage`
 * unwrapping the `mutate`/`runOptimisticMutation` wrap) is unchanged
 * from when this threw a plain `Error`.
 */
class GhMutationFailed extends Data.TaggedError("GhMutationFailed")<{
  readonly reason: string;
}> {
  override get message(): string {
    return this.reason;
  }
}
const ghFail = (reason: string): Effect.Effect<never, GhMutationFailed> =>
  Effect.fail(new GhMutationFailed({ reason }));

export type GithubPrFlowsCtx = {
  rows: readonly WorktreeRow[];
  toast: (message: string, color?: string, ms?: number) => void;
  mutate: <TData>(opts: {
    filter: QueryFilters;
    patch: (prev: TData | undefined) => TData | undefined;
    run: (() => Promise<void>) | Effect.Effect<void, unknown>;
  }) => Promise<void>;
  /**
   * Invalidate `["github"]`. Needed by the registration-gap retry,
   * which arms outside any `mutate` call and so has no settling
   * refetch of its own — without it the badge stays unarmed until the
   * slow staleTime expires.
   */
  refreshGithub: () => Promise<void>;
};

/** The current moment as an ISO string, off `Clock` rather than `Date.now()`. */
const nowIso = Effect.map(Clock.currentTimeMillis, (ms) =>
  DateTime.formatIso(DateTime.makeUnsafe(ms)),
);

export function makeGithubPrFlows(ctx: GithubPrFlowsCtx) {
  const { rows, toast, mutate, refreshGithub } = ctx;

  const markReady = Effect.fn("markReady")(function* (slug: string): Effect.fn.Return<void> {
    const log = createLogger(slug);
    const row = rows.find((r) => r.wt.slug === slug);
    if (!row?.pr) {
      toast("no PR for this row", theme.warn, 2000);
      return;
    }
    const prNumber = row.pr.number;
    const branch = row.wt.branch;
    const outcome = yield* io.promise("mark ready", () =>
      mutate<GithubData>({
        filter: { queryKey: ["github"] },
        patch: (data) =>
          patchPullRequest(data, branch, (pr) => ({ ...pr, isDraft: false })),
        run: markPullRequestReady(prNumber).pipe(
          Effect.flatMap((r) => (r.ok ? Effect.void : ghFail(r.error))),
        ),
      }),
    ).pipe(Effect.result);
    if (outcome._tag === "Failure") {
      const msg = causeMessage(outcome.failure.cause);
      log.event.err(`mark ready failed for #${prNumber}: ${msg}`);
      toast(`mark ready failed: ${msg}`, theme.err, 4000);
      return;
    }
    log.event.ok(`marked #${prNumber} ready for review`);
    toast(`marked #${prNumber} ready`, theme.ok, 2500);
  });

  function doMarkReady(slug: string): Promise<void> {
    return Effect.runPromise(markReady(slug));
  }

  /**
   * Toggle GitHub "merge when ready" on the PR. Which of the two
   * features that means is decided from the base branch when enabling,
   * and the PR's actual queue/auto-merge state when disabling. A PR on
   * a queue base may have classic auto-merge armed but not be queued.
   *
   * The optimistic patch flips `pr.autoMerge` either way, so on a queue
   * branch it paints the "armed" badge for the round trip even though
   * the truth will arrive as a queue POSITION instead. That is
   * deliberate: the point of the optimistic write is immediate
   * acknowledgement of a keystroke, and the settling invalidate replaces
   * it with whichever badge GitHub actually lands on.
   */
  const setAutoMerge = Effect.fn("setAutoMerge")(function* (
    subject: ActionSubject,
    action: "enable" | "disable",
  ): Effect.fn.Return<void> {
    const log = createLogger(subject.slug);
    const { pr } = subject;
    if (!pr) {
      toast("no PR for this row", theme.warn, 2000);
      return;
    }
    // Armed-ness spans BOTH features this keystroke can drive. Keying on
    // `pr.autoMerge` alone left a queued PR looking unarmed, so the disarm
    // leg refused to dequeue it.
    const armed = armedFromPr(pr, subject.mq);
    if (action === "enable" && armed) {
      toast("merge when ready already armed", theme.info, 2000);
      return;
    }
    // Nothing is armed while a pending-checks retry is waiting, so
    // without this the disarm leg would report "not armed" and leave
    // the loop running to arm it a minute later — the worst possible
    // answer to someone who just asked for it to stop.
    if (action === "disable" && cancelAutoMergeRetry(pr.number)) {
      log.event.ok(`#${pr.number}: cancelled the pending merge-when-ready arm`);
      toast("cancelled pending arm", theme.ok, 2500);
      return;
    }
    if (action === "enable" && autoMergeRetryPending(pr.number)) {
      toast("already waiting on checks", theme.info, 2000);
      return;
    }
    if (action === "disable" && !armed) {
      toast("merge when ready not armed", theme.info, 2000);
      return;
    }
    const prNumber = pr.number;
    const prId = pr.id;
    // The arm mutation needs the GraphQL node id, which persisted-cache
    // entries from before the field existed lack. One live fetch fills
    // it in — don't guess-and-merge with the number-based porcelain
    // (see enableAutoMerge's doc for why that command is banned).
    if (action === "enable" && prId === undefined) {
      toast("PR cache predates this build — press r, then retry", theme.warn, 3500);
      return;
    }
    const branch = subject.branch;
    // Optimistic shape for enable: seed the method the gh call will arm
    // (shared AUTO_MERGE_METHOD constant, so this can't drift from
    // enableAutoMerge). The invalidate that fires on success replaces it
    // with truth on the next refetch — what matters for UX is that the
    // badge flips immediately.
    const optimisticAutoMerge: PullRequest["autoMerge"] | null =
      action === "enable"
        ? { enabledAt: yield* nowIso, mergeMethod: AUTO_MERGE_METHOD }
        : null;
    // `retryable` rides a closure the way the pre-Effect code did (set
    // from inside the mutation's own Effect before it fails) — the
    // failure channel carries only what `mutate`'s optimistic wrapper
    // needs (`Error`), and this is the one bit downstream branching
    // needs back out.
    let retryable = false;
    const mutationEffect = (
      action === "enable"
        ? enableAutoMerge(prId ?? "", { baseRefName: pr.baseRefName, headRefOid: pr.headRefOid })
        : disableAutoMerge(prNumber, { prId, baseRefName: pr.baseRefName })
    ).pipe(
      Effect.tap((result) =>
        result.ok
          ? Effect.void
          : Effect.sync(() => {
              retryable = result.retryable === true;
            }),
      ),
      Effect.flatMap((result) => (result.ok ? Effect.void : ghFail(result.error))),
    );
    const outcome = yield* io.promise("auto-merge mutation", () =>
      mutate<GithubData>({
        filter: { queryKey: ["github"] },
        patch: (data) =>
          patchPullRequest(data, branch, (pr) => ({
            ...pr,
            autoMerge: optimisticAutoMerge,
          })),
        run: mutationEffect,
      }),
    ).pipe(Effect.result);

    if (outcome._tag === "Failure") {
      const msg = causeMessage(outcome.failure.cause);
      const verb = action === "enable" ? "auto-merge" : "disable auto-merge";
      if (retryable) {
        // A required check that has not reported yet — unregistered or
        // still running. Nothing about it needs a human, and pressing
        // the key again when CI finishes is exactly the work wt exists
        // to absorb. The retry re-sends the SAME `expectedHeadOid`, so
        // a push during the wait makes GitHub refuse rather than arming
        // a commit nobody saw.
        startAutoMergeRetry(
          prNumber,
          enableAutoMerge(prId ?? "", {
            baseRefName: pr.baseRefName,
            headRefOid: pr.headRefOid,
          }),
          {
            onArmed: () => {
              log.event.ok(`merge when ready armed for #${prNumber} once its checks reported`, {
                toast: true,
              });
              // Returned (not forked) — the retry fiber yields this
              // itself, so a failure here can't outlive the fiber that's
              // reporting it.
              return io.promise("refresh GitHub", refreshGithub).pipe(
                Effect.catch((error) =>
                  Effect.sync(() =>
                    log.event.err(
                      `GitHub refresh failed after arming #${prNumber}: ${causeMessage(error.cause)}`,
                    ),
                  ),
                ),
              );
            },
            onFailed: (error) => {
              log.event.err(`auto-merge failed for #${prNumber}: ${error}`, { toast: true });
            },
            onGaveUp: () => {
              log.event.err(
                `#${prNumber}: gave up arming merge when ready — a required check still had not reported after ${
                  RETRY_LIMIT_MS / 60_000
                } min, so check the workflow actually runs for this branch`,
                { toast: true },
              );
            },
          },
        );
        log.event.warn(
          `#${prNumber}: required checks have not reported yet — will arm merge when ready as soon as they do`,
        );
        toast(`#${prNumber}: waiting on checks, will arm`, theme.warn, 3500);
        return;
      }
      log.event.err(`${verb} failed for #${prNumber}: ${msg}`);
      toast(`${verb} failed: ${msg}`, theme.err, 4000);
      return;
    }
    const past = action === "enable" ? "enabled" : "disabled";
    log.event.ok(`auto-merge ${past} for #${prNumber}`);
    toast(`auto-merge ${past} for #${prNumber}`, theme.ok, 2500);
  });

  function doAutoMerge(
    subject: ActionSubject,
    action: "enable" | "disable",
  ): Promise<void> {
    return Effect.runPromise(setAutoMerge(subject, action));
  }

  /**
   * One-keystroke "ship it" (`E`): mark the PR ready, request
   * `config.github.defaultReviewer` if set, and arm auto-merge — in
   * the right order so GitHub doesn't reject the chain. Mark-ready
   * and reviewer-request run in parallel (no dependency); auto-merge
   * awaits mark-ready since `gh pr merge --auto` rejects drafts.
   * Each leg is idempotent: a re-press after partial failure only
   * re-runs the still-pending legs.
   */
  const shipPr = Effect.fn("shipPr")(function* (slug: string): Effect.fn.Return<void> {
    const log = createLogger(slug);
    const row = rows.find((r) => r.wt.slug === slug);
    if (!row?.pr) {
      toast("no PR for this row", theme.warn, 2000);
      return;
    }
    if (row.pr.state !== "OPEN") {
      toast("PR is not open", theme.warn, 2000);
      return;
    }
    const prNumber = row.pr.number;
    const prId = row.pr.id ?? null;
    const branch = row.wt.branch;
    const wasDraft = row.pr.isDraft;
    const reviewerToAdd =
      config.github.reviewers &&
      config.github.defaultReviewer &&
      !row.pr.requestedReviewers.includes(config.github.defaultReviewer)
        ? config.github.defaultReviewer
        : null;
    const needsAutoMerge = !row.pr.autoMerge;
    // Arm-only auto-merge needs the node id (absent from pre-field
    // persisted caches; one live fetch fills it in).
    if (needsAutoMerge && prId === null) {
      toast("PR cache predates this build — press r, then retry", theme.warn, 3500);
      return;
    }

    if (!wasDraft && !reviewerToAdd && !needsAutoMerge) {
      toast(`#${prNumber} already shipped`, theme.info, 2000);
      return;
    }
    const steps: string[] = [];
    if (wasDraft) steps.push("mark ready");
    if (reviewerToAdd) steps.push(`request ${reviewerToAdd}`);
    if (needsAutoMerge) steps.push("arm auto-merge");
    log.event.info(`ship #${prNumber}: ${steps.join(" + ")}`);

    const markReadyEffect: Effect.Effect<void, OperationError> = wasDraft
      ? io.promise("mark ready", () =>
          mutate<GithubData>({
            filter: { queryKey: ["github"] },
            patch: (data) =>
              patchPullRequest(data, branch, (pr) => ({ ...pr, isDraft: false })),
            run: markPullRequestReady(prNumber).pipe(
              Effect.flatMap((r) => (r.ok ? Effect.void : ghFail(r.error))),
            ),
          }),
        )
      : Effect.void;
    const reviewerEffect: Effect.Effect<void, OperationError> = reviewerToAdd
      ? io.promise("request reviewer", () =>
          mutate<GithubData>({
            filter: { queryKey: ["github"] },
            patch: (data) =>
              patchPullRequest(data, branch, (pr) => ({
                ...pr,
                requestedReviewers: [...pr.requestedReviewers, reviewerToAdd],
                reviewRequests: pr.reviewRequests + 1,
              })),
            run: editReviewers(prNumber, { add: [reviewerToAdd], remove: [] }).pipe(
              Effect.flatMap((r) => (r.ok ? Effect.void : ghFail(r.error))),
            ),
          }),
        )
      : Effect.void;

    const [readyRes, reviewerRes] = yield* Effect.all(
      [markReadyEffect.pipe(Effect.result), reviewerEffect.pipe(Effect.result)],
      { concurrency: 2 },
    );

    if (readyRes._tag === "Failure") {
      const msg = causeMessage(readyRes.failure.cause);
      log.event.err(`mark ready failed for #${prNumber}: ${msg}`);
      toast(`mark ready failed: ${msg}`, theme.err, 4000);
      // Bail: auto-merge would fail on the still-draft PR.
      return;
    }
    if (wasDraft) log.event.ok(`marked #${prNumber} ready`);

    if (reviewerRes._tag === "Failure") {
      const msg = causeMessage(reviewerRes.failure.cause);
      log.event.err(
        `request reviewer ${reviewerToAdd} failed for #${prNumber}: ${msg}`,
      );
      toast(`reviewer request failed: ${msg}`, theme.err, 4000);
      // Don't bail — auto-merge is independent of the reviewer request.
    } else if (reviewerToAdd) {
      log.event.ok(`requested ${reviewerToAdd} for #${prNumber}`);
    }

    if (needsAutoMerge) {
      const enabledAt = yield* nowIso;
      const armOutcome = yield* io.promise("enable auto-merge", () =>
        mutate<GithubData>({
          filter: { queryKey: ["github"] },
          patch: (data) =>
            patchPullRequest(data, branch, (pr) => ({
              ...pr,
              autoMerge: { enabledAt, mergeMethod: AUTO_MERGE_METHOD },
            })),
          run: enableAutoMerge(prId ?? "", {
            baseRefName: row.pr?.baseRefName,
            headRefOid: row.pr?.headRefOid,
          }).pipe(
            Effect.flatMap((r) => (r.ok ? Effect.void : ghFail(r.error))),
          ),
        }),
      ).pipe(Effect.result);
      if (armOutcome._tag === "Failure") {
        const msg = causeMessage(armOutcome.failure.cause);
        log.event.err(`auto-merge failed for #${prNumber}: ${msg}`);
        toast(`auto-merge failed: ${msg}`, theme.err, 4000);
        return;
      }
      log.event.ok(`auto-merge enabled for #${prNumber}`);
    }

    toast(`shipped #${prNumber}`, theme.ok, 2500);
  });

  function doShipPr(slug: string): Promise<void> {
    return Effect.runPromise(shipPr(slug));
  }

  /**
   * `f` — tail the failed-job logs of this PR's most recent failed CI run
   * into the activity pane (`gh run view --log-failed`), so a red check
   * can be diagnosed without a browser trip. Refuses when the PR's checks
   * aren't failing. Output is capped so a giant log can't flood the pane;
   * the tail keeps draining silently past the cap.
   */
  const tailFailedChecks = Effect.fn("tailFailedChecks")(function* (
    slug: string,
  ): Effect.fn.Return<void> {
    const log = createLogger(slug);
    const row = rows.find((r) => r.wt.slug === slug);
    if (!row?.pr) {
      toast("no PR for this row", theme.warn, 2000);
      return;
    }
    if (row.pr.checks !== "fail") {
      toast("no failing checks", theme.info, 2000);
      return;
    }
    const branch = row.wt.branch;
    // `?? []` guards a stale persisted PR (disk cache) from before this
    // field existed; the log fetch below still works off the branch alone.
    const names = row.pr.failedChecks ?? [];
    log.event.warn(
      `failing: ${names.length > 0 ? names.join(", ") : "checks"} — fetching logs…`,
    );
    toast("fetching failed CI logs…", theme.info, 2500);
    const CAP = 200;
    let emitted = 0;
    const res = yield* streamFailedRunLog(branch, (line) => {
      if (emitted < CAP) log.event.dim(line);
      else if (emitted === CAP) {
        log.event.dim(`… (truncated at ${CAP} lines; \`gh run view --log-failed\` for the rest)`);
      }
      emitted++;
    });
    if (!res.ok) {
      log.event.err(`failed CI logs: ${res.reason}`);
      toast(`failed logs: ${res.reason}`, theme.err, 4000);
      return;
    }
    log.event.ok(`failed CI logs for ${branch} (${Math.min(emitted, CAP)} shown)`);
  });

  function doTailFailedChecks(slug: string): Promise<void> {
    return Effect.runPromise(tailFailedChecks(slug));
  }

  return {
    doMarkReady,
    doAutoMerge,
    doShipPr,
    doTailFailedChecks,
    markReady,
    setAutoMerge,
    shipPr,
    tailFailedChecks,
  };
}
