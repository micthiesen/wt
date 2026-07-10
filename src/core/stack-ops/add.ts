import { config } from "../config.ts";
import { branchExists, gitRun, localOrOriginRef, originBranchExists } from "../git.ts";
import { createDraftPr, viewPrInfo } from "../github.ts";
import { dirSlug } from "../stage.ts";
import {
  findStackIdByBranch,
  getStackManifest,
  putStackManifest,
  readWtState,
  setSlugBase,
  validateStackManifest,
  type StackManifest,
  type StackSlice,
} from "../wtstate.ts";
import { fetchOrigin } from "../worktree.ts";
import { acquireStackLock, log, retargetIfNeeded, STACK_BUSY, type Logger } from "./shared.ts";

// ---------- add (append an existing branch to a live stack) ----------

export type AddSliceResult =
  | {
      ok: true;
      slice: StackSlice;
      /** Branch the new slice stacks on (a sibling's branch, or trunk). */
      parentBranch: string;
      /** Whether the slice's PR pre-existed or `add` opened it. */
      prAction: "adopted" | "created";
    }
  | { ok: false; error: string };

/** Fallback slice title from a branch name: strip the namespace + issue-id
 *  (+ optional ordinal) prefix, de-kebab the rest. `--title` overrides. */
function titleFromBranch(branch: string): string {
  const tail = branch.split("/").pop() ?? branch;
  const stripped = tail.replace(/^[a-z]+-\d+-(\d+[a-z]?-)?/i, "").replace(/-/g, " ").trim();
  return stripped || tail;
}

/**
 * Append an EXISTING branch to a live stack as a new tip slice — the inverse
 * of `splitStack`'s reshape, and the registration path for "I `wt new --base
 * <tip>`'d a branch on top of the stack, now track it". Purely additive
 * (existing slices untouched), which is what lets it work on a materialized
 * stack — the re-ingest path refuses those wholesale.
 *
 * Never creates a branch or worktree (`wt new` owns that); errors when the
 * branch doesn't exist. DOES ensure the slice has a PR: adopts an open one,
 * else pushes + opens a draft PR against the parent. That isn't scope creep —
 * `validateStackManifest` hard-rejects `open` without a `pr`, and a `planned`
 * slice would later be re-materialized by `applyStack` from the HOLISTIC
 * branch, clobbering an externally-authored branch's content. PR-or-create is
 * what keeps `apply` permanently away from this slice.
 *
 * The squash-safe anchor (`baseSha`) is recorded as `merge-base(branch,
 * parent)` — not the parent's tip, which may have advanced since the fork and
 * wouldn't be an ancestor of the branch.
 */
export async function addSliceToStack(
  stackId: string,
  branch: string,
  opts: { onto?: string; title?: string },
  onLog: Logger,
): Promise<AddSliceResult> {
  const lock = await acquireStackLock("add");
  if (!lock) return { ok: false, error: STACK_BUSY };
  try {
    return await addSliceToStackLocked(stackId, branch, opts, onLog);
  } finally {
    lock.release();
  }
}

async function addSliceToStackLocked(
  stackId: string,
  branch: string,
  opts: { onto?: string; title?: string },
  onLog: Logger,
): Promise<AddSliceResult> {
  const manifest = getStackManifest(stackId);
  if (!manifest) return { ok: false, error: `no stack manifest: ${stackId}` };
  // Freshen origin before resolving refs, mirroring replay's
  // fetch-before-anchor discipline: the squash-safe `baseSha` anchor and
  // the derived `files` list are computed from merge-base against the
  // parent (or origin trunk for a lane root) — stale origin refs would
  // bake a wrong anchor into the manifest.
  try {
    await fetchOrigin();
  } catch (err) {
    return {
      ok: false,
      error: `${err instanceof Error ? err.message : String(err)}; refusing to anchor against possibly-stale refs`,
    };
  }
  const owner = findStackIdByBranch(branch);
  if (owner) {
    return { ok: false, error: `${branch} is already tracked by stack ${owner}` };
  }
  if (!(await branchExists(branch))) {
    return {
      ok: false,
      error: `branch ${branch} not found (local or origin) — \`add\` registers an existing branch; create it first (e.g. \`wt new … --base <parentTip>\`)`,
    };
  }

  // Resolve the parent: `--onto` names a sibling slice (by id or branch) or
  // trunk (a new parallel lane root); default is the stack tip — the
  // highest-ordinal live slice.
  let parentSlice: StackSlice | null = null;
  if (opts.onto && opts.onto !== config.branch.base) {
    parentSlice =
      manifest.slices.find((s) => s.id === opts.onto || s.branch === opts.onto) ?? null;
    if (!parentSlice) {
      return {
        ok: false,
        error: `--onto ${opts.onto} matches no slice in ${stackId} (pass a slice id, a slice branch, or ${config.branch.base})`,
      };
    }
    if (parentSlice.status === "merged") {
      return {
        ok: false,
        error: `slice ${parentSlice.id} is merged — stack on a live slice, or --onto ${config.branch.base}`,
      };
    }
  } else if (!opts.onto) {
    // Prefer the fork base `wt new --base` recorded for this branch's
    // worktree, when it names a live slice of this stack — that's the
    // actual parent the branch was cut from. Fall back to the stack tip.
    const recorded = readWtState().slugs[dirSlug(branch)]?.baseBranch;
    const recordedSlice = recorded
      ? manifest.slices.find(
          (s) => (s.branch === recorded || s.id === recorded) && s.status !== "merged",
        )
      : undefined;
    if (recordedSlice) {
      parentSlice = recordedSlice;
      onLog(`parent ${recordedSlice.id} (${recordedSlice.branch}) from the recorded fork base`);
    } else {
      parentSlice =
        manifest.slices
          .filter((s) => s.status !== "merged")
          .sort((a, b) => b.ordinal - a.ordinal)[0] ?? null;
    }
    if (!parentSlice) {
      return {
        ok: false,
        error: `stack ${stackId} has no live slices — pass --onto ${config.branch.base} to root a new lane`,
      };
    }
  }
  const parentBranch = parentSlice ? parentSlice.branch : config.branch.base;

  const branchRef = await localOrOriginRef(branch);
  const parentRef = parentSlice
    ? await localOrOriginRef(parentBranch)
    : `origin/${config.branch.base}`;

  // Anchor = the cut point the branch's own commits sit on.
  const mb = await gitRun(["merge-base", branchRef, parentRef]);
  const baseSha = mb.stdout.trim();
  if (mb.exitCode !== 0 || !baseSha) {
    return {
      ok: false,
      error: `${branch} shares no history with ${parentBranch} — wrong parent? (--onto)`,
    };
  }

  // File partition = the branch's own diff vs the anchor. Doubles as the
  // empty-slice guard (nothing to review → nothing to track).
  const diff = await gitRun(["diff", "--name-only", `${baseSha}..${branchRef}`]);
  const files = diff.stdout.trim().split("\n").filter(Boolean);
  if (diff.exitCode !== 0 || files.length === 0) {
    return { ok: false, error: `${branch} has no changes on top of ${parentBranch} (empty slice)` };
  }

  // Ensure the PR. A CLOSED PR is not adopted — gh happily opens a fresh one
  // on the same branch, and the closed PR wasn't tracking this work anyway.
  let prNumber: number;
  let prAction: "adopted" | "created";
  let title = opts.title ?? "";
  const existing = await viewPrInfo(branch);
  if (existing?.state === "MERGED") {
    return {
      ok: false,
      error: `PR #${existing.number} for ${branch} is already merged — nothing left to stack`,
    };
  }
  if (existing && existing.state === "OPEN") {
    prNumber = existing.number;
    prAction = "adopted";
    title = title || existing.title;
    onLog(`adopted existing PR #${existing.number}`);
  } else {
    if (!(await originBranchExists(branch))) {
      const push = await gitRun(["push", "-u", "origin", branch]);
      if (push.exitCode !== 0) {
        return { ok: false, error: `push ${branch}: ${(push.stderr || push.stdout).trim()}` };
      }
      onLog(`pushed ${branch}`);
    }
    title = title || titleFromBranch(branch);
    const pr = await createDraftPr({
      cwd: config.paths.mainClone,
      head: branch,
      base: parentBranch,
      title,
      body: `Stacked on \`${parentBranch}\`.`,
    });
    if (!pr.ok) return { ok: false, error: `pr create ${branch}: ${pr.error}` };
    prNumber = pr.number;
    prAction = "created";
    onLog(`opened draft PR #${pr.number} (base ${parentBranch})`);
  }

  const maxOrdinal = Math.max(0, ...manifest.slices.map((s) => s.ordinal));
  const ids = new Set(manifest.slices.map((s) => s.id));
  let n = maxOrdinal + 1;
  while (ids.has(`s${n}`)) n++;

  const slice: StackSlice = {
    id: `s${n}`,
    ordinal: maxOrdinal + 1,
    title: title || titleFromBranch(branch),
    branch,
    base: parentSlice ? parentSlice.id : config.branch.base,
    dependsOn: parentSlice ? [parentSlice.id] : [],
    files,
    pr: prNumber,
    status: "open",
    oversized: false,
    baseSha,
  };

  const next: StackManifest = { ...manifest, slices: [...manifest.slices, slice] };
  const v = validateStackManifest(next);
  if (!v.ok) {
    return { ok: false, error: `manifest invalid after add:\n  ${v.errors.join("\n  ")}` };
  }
  putStackManifest(v.manifest);

  // An adopted PR may target the wrong base (e.g. trunk, because `gh pr
  // create` defaulted there) — align it with the manifest like replay does.
  if (prAction === "adopted") await retargetIfNeeded(slice, parentBranch, onLog);

  // The branch is a tracked slice now — the manifest owns its parent, so
  // the `wt new --base` fork-base hint is superseded. (Stack lock is held;
  // the wtstate flock nests inside it, consistent with every other mutator.)
  setSlugBase(dirSlug(branch), null);

  log.info("added slice to stack", { stackId, slice: slice.id, branch, parentBranch, prNumber });
  return { ok: true, slice, parentBranch, prAction };
}
