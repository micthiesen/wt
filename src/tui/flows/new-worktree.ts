/**
 * Worktree-creation flows: the `n`/`N` prompt (doNew), review checkout
 * (`w` on a review-request row), and removed-history restore (Enter in
 * the `h` view). Extracted from `app.tsx`; rebuilt per render so the
 * closures see fresh setters.
 */
import { config } from "../../core/config.ts";
import { createWorktree, parseInput } from "../../core/lifecycle.ts";
import { createLogger } from "../../core/logger.ts";
import { runRemoteWt } from "../../core/remote.ts";
import type { RemovedWorktree } from "../../core/wtstate.ts";
import { parseNewInput } from "../app-helpers.ts";
import type { Modal } from "../modal-state.ts";
import type { RemoteCreation } from "../remote-creation.ts";
import { theme } from "../theme.ts";

const newLog = createLogger("[new]");

/** Section a review-requested PR lands in when checked out via `w`. */
export const REVIEW_SECTION = "Reviews";

type WorktreeCreateFlowsCtx = {
  setModal: (m: Modal | null) => void;
  setSection: (slug: string, section: string | null) => Promise<void>;
  setSel: (key: string | null) => void;
  setRemovedView: (v: boolean) => void;
  setRemoteCreation: (creation: RemoteCreation | null) => void;
  refreshAll: () => Promise<void>;
  refreshRemoteWorktrees: () => Promise<void>;
  toast: (message: string, color?: string, ms?: number) => void;
};

export function makeWorktreeCreateFlows(ctx: WorktreeCreateFlowsCtx) {
  const {
    setModal,
    setSection,
    setSel,
    setRemovedView,
    setRemoteCreation,
    refreshAll,
    refreshRemoteWorktrees,
    toast,
  } = ctx;

  async function doNew(raw: string, defaultBase?: string): Promise<void> {
    const parsed = parseNewInput(raw, defaultBase);
    if ("error" in parsed) {
      newLog.event.err(parsed.error);
      return;
    }
    newLog.event.info(`resolving ${parsed.input}`);
    if (parsed.anyAuthor) newLog.event.info("searching all authors (--any)");
    if (parsed.base) newLog.event.info(`base: ${parsed.base}`);
    let branch: string;
    try {
      branch = await parseInput(parsed.input, {
        anyAuthor: parsed.anyAuthor,
        promptForChoice: (id, branches) =>
          new Promise<string | null>((resolve) => {
            setModal({
              kind: "branchPicker",
              title: `multiple branches for ${id}`,
              items: branches,
              index: 0,
              resolve,
            });
          }),
      });
    } catch (err) {
      newLog.event.err(err instanceof Error ? err.message : String(err));
      newLog.error(err instanceof Error ? err : String(err));
      return;
    }
    newLog.event.info(`branch = ${branch}`);
    const result = await createWorktree(branch, {
      onPhase: (p) => newLog.event.info(`phase: ${p}`),
      onLog: (line) => newLog.event.dim(line),
      runInstall: true,
      base: parsed.base,
    });
    if (!result.ok) {
      newLog.event.err(result.reason);
      return;
    }
    newLog.event.ok(`ready at ${result.path}`);
    void refreshAll();
  }

  /** Create on the remote host, then refresh its rows in this TUI. */
  async function doRemoteNew(raw: string): Promise<void> {
    const remote = config.remote;
    if (!remote) {
      toast("[remote] is not configured", theme.warn, 2200);
      return;
    }
    const parsed = parseNewInput(raw);
    if ("error" in parsed) {
      newLog.event.err(parsed.error);
      return;
    }
    const args = ["new", parsed.input, "--no-open"];
    if (parsed.anyAuthor) args.push("--any");
    if (parsed.base) args.push("--base", parsed.base);

    const remoteLog = createLogger(`[remote:${remote.label}]`);
    setRemoteCreation({
      hostLabel: remote.label,
      input: parsed.input,
      status: "creating",
    });
    remoteLog.event.info(`creating ${parsed.input}`);
    // The normal remote inventory interval is 15s while no busy row is known.
    // Probe eagerly during creation so the authoritative row replaces the
    // placeholder as soon as the checkout exists; F10/F11/F12 can then enter
    // it while the remaining init phases continue in the background.
    let refreshStopped = false;
    let refreshTimer: ReturnType<typeof setTimeout> | null = null;
    const pollRemote = async (): Promise<void> => {
      await refreshRemoteWorktrees().catch(() => undefined);
      if (!refreshStopped) {
        refreshTimer = setTimeout(() => void pollRemote(), 1_500);
      }
    };
    void pollRemote();
    let code: number;
    try {
      code = await runRemoteWt(remote, args, {
        onLine: (line) => remoteLog.event.dim(line),
      });
    } catch (err) {
      const message = err instanceof Error ? err.message : String(err);
      remoteLog.event.err(message);
      toast(`remote create failed: ${message}`, theme.err, 3500);
      setRemoteCreation(null);
      return;
    } finally {
      refreshStopped = true;
      if (refreshTimer) clearTimeout(refreshTimer);
    }
    if (code !== 0) {
      remoteLog.event.err(`create failed (exit ${code})`);
      toast(`remote create failed (exit ${code})`, theme.err, 3000);
      setRemoteCreation(null);
      return;
    }
    remoteLog.event.ok(`ready on ${remote.label}`);
    try {
      await refreshRemoteWorktrees();
      toast(`ready on ${remote.label}`, theme.ok, 1800);
    } finally {
      setRemoteCreation(null);
    }
  }

  // Check out a review-requested PR's branch as a worktree and drop it
  // into the "Reviews" section. The branch already exists on origin, so
  // `createWorktree` takes the checkout-existing path (sets upstream,
  // installs packages); `setSection` materializes the section by simply
  // assigning the new slug to it. Leaves the review-request row in place
  // — this spawns a worktree, it doesn't consume the PR.
  async function doCheckoutReview(branch: string): Promise<void> {
    const log = createLogger("[review]");
    log.event.info(`creating review worktree for ${branch}`);
    const result = await createWorktree(branch, {
      onPhase: (p) => log.event.info(`phase: ${p}`),
      onLog: (line) => log.event.dim(line),
      runInstall: true,
    });
    if (!result.ok) {
      log.event.err(result.reason);
      toast(`worktree failed: ${result.reason}`, theme.err, 3000);
      return;
    }
    await setSection(result.slug, REVIEW_SECTION);
    log.event.ok(`ready at ${result.path} → ${REVIEW_SECTION}`);
    toast(`created ${result.slug} in ${REVIEW_SECTION}`, theme.info, 2200);
    void refreshAll();
  }

  // Restore a removed worktree: a real `createWorktree` for the recorded
  // branch. If the branch still exists (locally or on origin) this checks
  // it out; if it's fully gone (merged + deleted) it starts a fresh branch
  // of the same name off trunk. `createWorktree` clears the removed-history
  // entry itself, so success just needs to land the cursor on the new row.
  async function doRestoreRemoved(entry: RemovedWorktree): Promise<void> {
    const log = createLogger("[restore]");
    log.event.info(`restoring ${entry.slug} (${entry.branch})`);
    const result = await createWorktree(entry.branch, {
      onPhase: (p) => log.event.info(`phase: ${p}`),
      onLog: (line) => log.event.dim(line),
      runInstall: true,
    });
    if (!result.ok) {
      log.event.err(result.reason);
      toast(`restore failed: ${result.reason}`, theme.err, 3000);
      return;
    }
    log.event.ok(`restored at ${result.path}`);
    toast(`restored ${result.slug}`, theme.ok, 2500);
    setRemovedView(false);
    setSel(result.slug);
    void refreshAll();
  }

  return { doNew, doRemoteNew, doCheckoutReview, doRestoreRemoved };
}
