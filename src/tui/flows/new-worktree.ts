/**
 * Worktree-creation flows: the `n`/`N` prompt (doNew), review checkout
 * (`w` on a review-request row), and removed-history restore (Enter in
 * the `h` view). Extracted from `app.tsx`; rebuilt per render so the
 * closures see fresh setters.
 *
 * Each keystroke entrypoint stays a thin `Effect.runPromise` wrapper —
 * kept Promise-returning for the footer-input / removed-view consumers
 * — running the exported Effect (`createNewWorktree`, …) that holds the
 * actual logic.
 */
import { config } from "../../core/config.ts";
import { Effect } from "effect";
import {
  createWorktree,
  parseInput,
  type CreateOptions,
  type CreateResult,
} from "../../core/lifecycle.ts";
import { operationErrors, type OperationError } from "../../core/errors.ts";
import { createLogger } from "../../core/logger.ts";
import { runRemoteWt } from "../../core/remote.ts";
import type { RemoteWorktreeSummary } from "../../core/remote-worktrees.ts";
import type { HarnessId } from "../../core/harness/index.ts";
import { readWtState, GROUP_INBOX, setSlugGithubIssue, type RemovedWorktree } from "../../core/wtstate.ts";
import { parseNewInput } from "../app-helpers.ts";
import type { Modal } from "../modal-state.ts";
import {
  discoveredRemoteCreation,
  consumeRemoteCreationSession,
  remoteEntryKey,
  type RemoteCreation,
} from "../remote-creation.ts";
import type { CreatedWorktreePlacement } from "../created-worktree.ts";
import { remoteWorktreeLedgerKey } from "../../core/worktree-ref.ts";
import { theme } from "../theme.ts";

const newLog = createLogger("[new]");

const io = operationErrors("new worktree flows");

/** Section a review-requested PR lands in when checked out via `w`. */
export const REVIEW_SECTION = "Reviews";

type WorktreeCreateFlowsCtx = {
  setModal: (m: Modal | null) => void;
  setSection: (slug: string, section: string | null) => Promise<void>;
  revealCreated: (placement: CreatedWorktreePlacement) => void;
  setSectionFolded: (key: string, folded: boolean) => Promise<boolean>;
  setRemovedView: (v: boolean) => void;
  setRemoteCreation: (creation: RemoteCreation | null) => void;
  remoteWorktrees: readonly RemoteWorktreeSummary[];
  refreshAfterCreation: (slug: string) => Promise<void>;
  refreshRemoteWorktrees: () => Promise<readonly RemoteWorktreeSummary[]>;
  enterCreatedRemoteHarness: (row: RemoteWorktreeSummary, harnessId: HarnessId) => void;
  toast: (message: string, color?: string, ms?: number) => void;
};

export function makeWorktreeCreateFlows(ctx: WorktreeCreateFlowsCtx) {
  const {
    setModal,
    setSection,
    revealCreated,
    setSectionFolded,
    setRemovedView,
    setRemoteCreation,
    remoteWorktrees,
    refreshAfterCreation,
    refreshRemoteWorktrees,
    toast,
  } = ctx;

  const revealLocal = Effect.fn("revealCreatedLocalWorktree")(function* (slug: string) {
    const state = yield* io.sync("read created section", () => readWtState().slugs[slug]);
    const section = state?.section ?? null;
    yield* io.promise("append created worktree", () => setSection(slug, section));
    yield* io.promise("expand created section", () =>
      setSectionFolded(section ?? GROUP_INBOX, false),
    );
    const order = yield* io.sync("read created order", () => readWtState().slugs[slug]!.order);
    revealCreated({ key: slug, ledgerKey: slug, section, workAt: state?.work?.at, order });
  });

  /**
   * `createWorktree` fails typed with `LifecycleError`; every caller
   * here wants the `CreateResult` union instead (same fold
   * `createWorktreePromise` used to do at the core boundary), so the
   * ok/failed branching below is unchanged.
   */
  const createWorktreeResult = (
    branch: string,
    opts: CreateOptions,
  ): Effect.Effect<CreateResult> =>
    createWorktree(branch, opts).pipe(
      Effect.catchTag("LifecycleError", (error) =>
        Effect.succeed({ ok: false as const, reason: error.message }),
      ),
    );

  // Effect body of `doNew`. Returns whether the create succeeded — the
  // footer-input handler awaits this to decide between the optimistic
  // legend reset (success) and restoring the typed line for editing in
  // place (failure). Never fails: every error path is reported (log +
  // toast) and folded into `false`.
  const createNewWorktree = Effect.fn("createNewWorktree")(function* (
    raw: string,
    defaultBase?: string,
  ): Effect.fn.Return<boolean, OperationError> {
    const parsed = parseNewInput(raw, defaultBase);
    if ("error" in parsed) {
      newLog.event.err(parsed.error);
      toast(parsed.error, theme.err, 3000);
      return false;
    }
    newLog.event.info(`resolving ${parsed.input}`);
    if (parsed.anyAuthor) newLog.event.info("searching all authors (--any)");
    if (parsed.attach) newLog.event.info("attaching to an existing branch (--attach)");
    if (parsed.base) newLog.event.info(`base: ${parsed.base}`);
    const branch = yield* parseInput(parsed.input, {
      anyAuthor: parsed.anyAuthor,
      attach: parsed.attach,
      // `parseInput` accepts either a Promise or an Effect here —
      // `Effect.callback` wraps the modal's choice-resolution
      // callback without a bare `new Promise`.
      promptForChoice: (id, branches) =>
        Effect.callback<string | null>((resume) => {
          setModal({
            kind: "branchPicker",
            title: `multiple branches for ${id}`,
            items: branches,
            index: 0,
            resolve: (choice) => resume(Effect.succeed(choice)),
          });
        }),
    }).pipe(
      Effect.catchTag("ParseInputError", (error) =>
        Effect.sync(() => {
          newLog.event.err(error.message);
          newLog.error(error.message, { cause: error.cause });
          toast(error.message, theme.err, 3000);
          return null;
        }),
      ),
    );
    if (branch === null) return false;
    newLog.event.info(`branch = ${branch}`);
    const result = yield* createWorktreeResult(branch, {
      onPhase: (p) => newLog.event.info(`phase: ${p}`),
      onLog: (line) => newLog.event.dim(line),
      runInstall: true,
      base: parsed.base,
    });
    if (!result.ok) {
      // Attention-channel: a create failure is an async completion whose
      // trigger keystroke is long past, and it needs acting on (fix the
      // main clone, retry). The emit toasts by default — no ctx.toast on
      // top, per the single-emit contract.
      newLog.attention.err(`worktree failed: ${result.reason}`);
      return false;
    }
    if (parsed.gh) {
      setSlugGithubIssue(result.slug, parsed.gh);
      newLog.event.info(`gh issue: #${parsed.gh}`);
    }
    newLog.event.ok(`ready at ${result.path}`);
    toast(`created ${result.slug}`, theme.ok, 2200);
    yield* revealLocal(result.slug);
    void refreshAfterCreation(result.slug);
    return true;
  });

  function doNew(raw: string, defaultBase?: string): Promise<boolean> {
    return Effect.runPromise(createNewWorktree(raw, defaultBase));
  }

  /**
   * Effect body of `doRemoteNew` — create on the remote host, then
   * refresh its rows in this TUI. Returns whether it succeeded — same
   * contract as `createNewWorktree`. Releases the visibility hold on every exit.
   */
  const createRemoteWorktree = Effect.fn("createRemoteWorktree")(function* (
    raw: string,
  ): Effect.fn.Return<boolean, OperationError> {
    const remote = config.remote;
    if (!remote) {
      toast("[remote] is not configured", theme.warn, 2200);
      return false;
    }
    const parsed = parseNewInput(raw);
    if ("error" in parsed) {
      newLog.event.err(parsed.error);
      return false;
    }
    const args = ["new", parsed.input, "--no-open"];
    if (parsed.anyAuthor) args.push("--any");
    if (parsed.attach) args.push("--attach");
    if (parsed.gh) args.push("--gh", String(parsed.gh));
    if (parsed.base) args.push("--base", parsed.base);

    const remoteLog = createLogger(`[remote:${remote.label}]`);
    const previousKeys = new Set(remoteWorktrees.map(remoteEntryKey));
    const creation: RemoteCreation = {
      remote,
      hostKey: remote.host,
      hostLabel: remote.label,
      input: parsed.input,
      previousKeys: [...previousKeys],
      status: "creating",
    };
    setRemoteCreation(creation);
    remoteLog.event.info(`creating ${parsed.input}`);
    toast(`creating ${parsed.input} on ${remote.label}`, theme.info, 2500);

    return yield* Effect.gen(function* () {
      yield* io.promise("expand inbox for remote creation", () =>
        setSectionFolded(GROUP_INBOX, false),
      );
      const code = yield* runRemoteWt(remote, args, {
        onLine: (line) => remoteLog.event.dim(line),
      }).pipe(
        Effect.catchTag("RemoteRunError", (error) =>
          Effect.sync(() => {
            remoteLog.event.err(error.message);
            toast(`remote create failed: ${error.message}`, theme.err, 3500);
            return null;
          }),
        ),
      );
      if (code === null) return false;
      if (code !== 0) {
        remoteLog.event.err(`create failed (exit ${code})`);
        toast(`remote create failed (exit ${code})`, theme.err, 3000);
        return false;
      }
      remoteLog.event.ok(`ready on ${remote.label}`);
      creation.status = "ready";
      const refreshed = yield* io.promise("refresh remote worktrees", refreshRemoteWorktrees);
      const created = discoveredRemoteCreation(creation, refreshed);
      if (created) {
        const ledgerKey = remoteWorktreeLedgerKey(created.hostKey, created.slug);
        const section = yield* io.sync("read created remote section", () =>
          readWtState().remoteLayouts[ledgerKey]?.section ?? null,
        );
        yield* io.promise("append created remote worktree", () => setSection(ledgerKey, section));
        yield* io.promise("expand created section", () =>
          setSectionFolded(section ?? GROUP_INBOX, false),
        );
        const order = yield* io.sync("read created remote order", () =>
          readWtState().remoteLayouts[ledgerKey]!.order,
        );
        revealCreated({ key: `remote:${remoteEntryKey(created)}`, ledgerKey, section, workAt: created.work?.at, order });
        const requestedHarness = consumeRemoteCreationSession(creation, created);
        if (requestedHarness) {
          yield* io.sync("enter requested remote session", () =>
            ctx.enterCreatedRemoteHarness(created, requestedHarness),
          );
        }
      }
      setRemoteCreation(null);
      if (!created && creation.requestedHarness) {
        remoteLog.event.warn("created checkout was not found; agent session was not opened");
        toast("created checkout was not found; select it after refresh and press F12", theme.warn, 3500);
      } else {
        toast(`ready on ${remote.label}`, theme.ok, 1800);
      }
      return true;
    }).pipe(Effect.ensuring(Effect.sync(() => setRemoteCreation(null))));
  });

  function doRemoteNew(raw: string): Promise<boolean> {
    return Effect.runPromise(createRemoteWorktree(raw));
  }

  // Check out a review-requested PR's branch as a worktree and drop it
  // into the "Reviews" section. The branch already exists on origin, so
  // `createWorktree` takes the checkout-existing path (sets upstream,
  // installs packages); `setSection` materializes the section by simply
  // assigning the new slug to it. Leaves the review-request row in place
  // — this spawns a worktree, it doesn't consume the PR.
  const checkoutReviewWorktree = Effect.fn("checkoutReviewWorktree")(function* (
    branch: string,
  ): Effect.fn.Return<void, OperationError> {
    const log = createLogger("[review]");
    log.event.info(`creating review worktree for ${branch}`);
    const result = yield* createWorktreeResult(branch, {
      onPhase: (p) => log.event.info(`phase: ${p}`),
      onLog: (line) => log.event.dim(line),
      runInstall: true,
    });
    if (!result.ok) {
      // Attention-channel (toasts by default) — see doNew's failure path.
      log.attention.err(`worktree failed: ${result.reason}`);
      return;
    }
    yield* io.promise("set section", () => setSection(result.slug, REVIEW_SECTION));
    log.event.ok(`ready at ${result.path} → ${REVIEW_SECTION}`);
    toast(`created ${result.slug} in ${REVIEW_SECTION}`, theme.info, 2200);
    yield* revealLocal(result.slug);
    void refreshAfterCreation(result.slug);
  });

  function doCheckoutReview(branch: string): Promise<void> {
    return Effect.runPromise(checkoutReviewWorktree(branch));
  }

  // Restore a removed worktree: a real `createWorktree` for the recorded
  // branch. If the branch still exists (locally or on origin) this checks
  // it out; if it's fully gone (merged + deleted) it starts a fresh branch
  // of the same name off trunk. `createWorktree` clears the removed-history
  // entry itself, so success just needs to land the cursor on the new row.
  const restoreRemovedWorktree = Effect.fn("restoreRemovedWorktree")(function* (
    entry: RemovedWorktree,
  ): Effect.fn.Return<void, OperationError> {
    const log = createLogger("[restore]");
    log.event.info(`restoring ${entry.slug} (${entry.branch})`);
    const result = yield* createWorktreeResult(entry.branch, {
      onPhase: (p) => log.event.info(`phase: ${p}`),
      onLog: (line) => log.event.dim(line),
      runInstall: true,
    });
    if (!result.ok) {
      // Attention-channel (toasts by default) — see doNew's failure path.
      log.attention.err(`restore failed: ${result.reason}`);
      return;
    }
    log.event.ok(`restored at ${result.path}`);
    toast(`restored ${result.slug}`, theme.ok, 2500);
    setRemovedView(false);
    yield* revealLocal(result.slug);
    void refreshAfterCreation(result.slug);
  });

  function doRestoreRemoved(entry: RemovedWorktree): Promise<void> {
    return Effect.runPromise(restoreRemovedWorktree(entry));
  }

  return {
    doNew,
    doRemoteNew,
    doCheckoutReview,
    doRestoreRemoved,
    createNewWorktree,
    createRemoteWorktree,
    checkoutReviewWorktree,
    restoreRemovedWorktree,
  };
}
