import { config } from "../config.ts";
import { createLogger } from "../logger.ts";
import { run, runStreaming } from "../proc.ts";
import type { AutoMergeMethod } from "../types.ts";
import { hasGh } from "./gh-cli.ts";
import type {
  EnableAutoMergeResult,
  GhActionResult,
  LivePrInfo,
} from "./types.ts";

const log = createLogger("[gh]");

/**
 * The merge method `enableAutoMerge` arms. Hardcoded to match the repo's
 * merge style; promote to config when there's a second concrete
 * preference. Exported so the TUI's optimistic patch shows the same
 * method the gh call will actually use — the two must never drift.
 */
export const AUTO_MERGE_METHOD: AutoMergeMethod = "REBASE";

/**
 * Enable "merge when ready" on a PR via `gh pr merge --auto`. Runs from
 * the main clone so gh resolves the right repo. `gh` does the right
 * thing for both classic auto-merge and merge-queue repos — the same
 * flag enqueues when a queue is configured.
 */
export async function enableAutoMerge(
  prNumber: number,
): Promise<EnableAutoMergeResult> {
  if (!(await hasGh())) return { ok: false, error: "gh CLI not found" };
  const r = await run(
    ["gh", "pr", "merge", String(prNumber), "--auto", `--${AUTO_MERGE_METHOD.toLowerCase()}`],
    { cwd: config.paths.mainClone, timeoutMs: 15_000 },
  );
  if (r.exitCode !== 0) {
    const msg = (r.stderr || r.stdout).trim() || `gh exited ${r.exitCode}`;
    log.error("auto-merge failed", { prNumber, msg });
    return { ok: false, error: msg };
  }
  return { ok: true };
}

/**
 * Cancel a previously-armed "merge when ready" via
 * `gh pr merge --disable-auto`. No-op on PRs that aren't currently
 * armed; gh returns an error in that case which we surface verbatim.
 */
export async function disableAutoMerge(
  prNumber: number,
): Promise<GhActionResult> {
  if (!(await hasGh())) return { ok: false, error: "gh CLI not found" };
  const r = await run(
    ["gh", "pr", "merge", String(prNumber), "--disable-auto"],
    { cwd: config.paths.mainClone, timeoutMs: 15_000 },
  );
  if (r.exitCode !== 0) {
    const msg = (r.stderr || r.stdout).trim() || `gh exited ${r.exitCode}`;
    log.error("disable auto-merge failed", { prNumber, msg });
    return { ok: false, error: msg };
  }
  return { ok: true };
}

/**
 * Edit a PR's review requests via `gh pr edit`. Both `add` and
 * `remove` may be passed in the same call — gh accepts both flag
 * sets at once. Logins are users; team slugs use the `org/team-slug`
 * form. Empty changes is a no-op.
 */
export async function editReviewers(
  prNumber: number,
  changes: { add: readonly string[]; remove: readonly string[] },
): Promise<GhActionResult> {
  if (changes.add.length === 0 && changes.remove.length === 0) {
    return { ok: true };
  }
  if (!(await hasGh())) return { ok: false, error: "gh CLI not found" };
  const argv = ["gh", "pr", "edit", String(prNumber)];
  for (const l of changes.add) argv.push("--add-reviewer", l);
  for (const l of changes.remove) argv.push("--remove-reviewer", l);
  const r = await run(argv, { cwd: config.paths.mainClone, timeoutMs: 15_000 });
  if (r.exitCode !== 0) {
    const msg = (r.stderr || r.stdout).trim() || `gh exited ${r.exitCode}`;
    log.error("edit reviewers failed", { prNumber, changes, msg });
    return { ok: false, error: msg };
  }
  return { ok: true };
}

/**
 * Retarget a PR's base branch via `gh pr edit --base`. The native restack
 * engine calls this after replaying a branch whose parent moved (e.g. a
 * child reparented onto trunk once its parent landed), so the PR's base on
 * GitHub matches the recorded parent. No-op-safe: gh is idempotent if the base
 * already matches. Runs from the main clone so gh resolves the right repo.
 */
export async function retargetPrBase(
  prNumber: number,
  base: string,
): Promise<GhActionResult> {
  if (!(await hasGh())) return { ok: false, error: "gh CLI not found" };
  const r = await run(
    ["gh", "pr", "edit", String(prNumber), "--base", base],
    { cwd: config.paths.mainClone, timeoutMs: 15_000 },
  );
  if (r.exitCode !== 0) {
    const msg = (r.stderr || r.stdout).trim() || `gh exited ${r.exitCode}`;
    log.error("retarget pr base failed", { prNumber, base, msg });
    return { ok: false, error: msg };
  }
  return { ok: true };
}

/**
 * Flip a draft PR to "ready for review" via `gh pr ready`. Notifies
 * reviewers and triggers any code-owner auto-requests, so callers
 * should gate on user confirmation. Runs from the main clone so gh
 * resolves the right repo.
 */
export async function markPullRequestReady(
  prNumber: number,
): Promise<GhActionResult> {
  if (!(await hasGh())) return { ok: false, error: "gh CLI not found" };
  const r = await run(
    ["gh", "pr", "ready", String(prNumber)],
    { cwd: config.paths.mainClone, timeoutMs: 15_000 },
  );
  if (r.exitCode !== 0) {
    const msg = (r.stderr || r.stdout).trim() || `gh exited ${r.exitCode}`;
    log.error("mark ready failed", { prNumber, msg });
    return { ok: false, error: msg };
  }
  return { ok: true };
}

/**
 * Stream the failed-job logs of the most recent failed CI run for
 * `branch` to `onLine`, via `gh run view <id> --log-failed`. Resolves
 * the count of lines emitted, or a reason when gh is missing, no failed
 * run exists (a check can fail as a bare `StatusContext` with no Actions
 * run behind it), or gh errors. Read-only; safe to fire from a keybind.
 */
export async function streamFailedRunLog(
  branch: string,
  onLine: (line: string) => void,
): Promise<{ ok: true } | { ok: false; reason: string }> {
  if (!(await hasGh())) return { ok: false, reason: "gh CLI not found" };
  const listed = await run(
    [
      "gh", "run", "list",
      "--branch", branch,
      "--status", "failure",
      "--limit", "1",
      "--json", "databaseId",
    ],
    { cwd: config.paths.mainClone, timeoutMs: 15_000 },
  );
  if (listed.exitCode !== 0) {
    return { ok: false, reason: (listed.stderr || listed.stdout).trim() || "gh run list failed" };
  }
  let runs: Array<{ databaseId?: number }>;
  try {
    runs = JSON.parse(listed.stdout) as typeof runs;
  } catch {
    return { ok: false, reason: "could not parse gh run list" };
  }
  const runId = runs[0]?.databaseId;
  if (runId === undefined) return { ok: false, reason: "no failed workflow run" };
  const code = await runStreaming(
    ["gh", "run", "view", String(runId), "--log-failed"],
    { cwd: config.paths.mainClone, onLine },
  );
  if (code !== 0) return { ok: false, reason: `gh run view exited ${code}` };
  return { ok: true };
}

/**
 * Read the live `baseRefName` / `state` for a branch's PR via
 * `gh pr view`. The restack reconcile/retarget paths use it to compare
 * the recorded parent against the PR's actual base. Returns null when
 * there's no PR (or gh is unavailable).
 */
export async function viewPrInfo(branch: string): Promise<LivePrInfo | null> {
  if (!branch || !(await hasGh())) return null;
  const r = await run(
    ["gh", "pr", "view", branch, "--json", "number,baseRefName,state,isDraft,title"],
    { cwd: config.paths.mainClone, timeoutMs: 15_000 },
  );
  if (r.exitCode !== 0) return null;
  try {
    const d = JSON.parse(r.stdout) as Partial<LivePrInfo>;
    if (typeof d.number !== "number") return null;
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
    };
  } catch {
    return null;
  }
}
