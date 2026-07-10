/**
 * Worktree-creation flows: the `n`/`N` prompt (doNew), review checkout
 * (`w` on a review-request row), and removed-history restore (Enter in
 * the `h` view). Extracted from `app.tsx`; rebuilt per render so the
 * closures see fresh setters.
 */
import { createWorktree, parseInput } from "../../core/lifecycle.ts";
import { createLogger } from "../../core/logger.ts";
import type { RemovedWorktree } from "../../core/wtstate.ts";
import { parseNewInput } from "../app-helpers.ts";
import type { Modal } from "../modal-state.ts";
import { theme } from "../theme.ts";

const newLog = createLogger("[new]");

/** Section a review-requested PR lands in when checked out via `w`. */
export const REVIEW_SECTION = "Reviews";

type WorktreeCreateFlowsCtx = {
  setModal: (m: Modal | null) => void;
  setSection: (slug: string, section: string | null) => Promise<void>;
  setSel: (key: string | null) => void;
  setRemovedView: (v: boolean) => void;
  refreshAll: () => Promise<void>;
  toast: (message: string, color?: string, ms?: number) => void;
};

export function makeWorktreeCreateFlows(ctx: WorktreeCreateFlowsCtx) {
  const { setModal, setSection, setSel, setRemovedView, refreshAll, toast } = ctx;

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

  return { doNew, doCheckoutReview, doRestoreRemoved };
}
