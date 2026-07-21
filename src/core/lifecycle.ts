import { closeSync, copyFileSync, existsSync, mkdirSync, openSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { clearArchived } from "./archive.ts";
import { clearClaudeNames } from "./harness/claude/names.ts";
import { clearCodexNames } from "./harness/codex/names.ts";
import { clearOpencodeNames } from "./harness/opencode/names.ts";
import {
  clearRemovedWorktree,
  clearSlugState,
  recordRemovedWorktrees,
  reparentBaseReferences,
  setSlugBase,
} from "./wtstate.ts";
import { getBackend, getBackendForPath } from "./backend.ts";
import { config } from "./config.ts";
import { branchExists, git, gitQuiet, originBranchExists, revParse } from "./git.ts";
import { LINEAR_ID_RE, LINEAR_URL_RE } from "./linear.ts";
import { lockLabel, lockStatus, tryAcquireLock } from "./locks.ts";
import { runStreaming } from "./proc.ts";
import { computeStage, dirSlug, slugify } from "./stage.ts";
import { safeStage } from "./stage-safety.ts";
import type { Worktree } from "./types.ts";
import { fetchOrigin } from "./worktree.ts";

/**
 * How long `removeWorktree` waits out a transient lock holder before
 * giving up. Sized to outlast a restack `reconcileStack`'s live `gh pr
 * view` probes (held under the chain lock), which the automation
 * clean-then-restack path races. Well short of a genuinely long-held
 * operation lock, which still fails so the destroy doesn't hang.
 */
const LOCK_ACQUIRE_WAIT_MS = 8000;

export type CreateResult =
  | { ok: true; path: string; branch: string; stage: string; slug: string }
  | { ok: false; reason: string };

/**
 * Return branches matching `<prefix>/<issue-id>(-|$)`. When `anyAuthor`
 * is set, `<prefix>` is any single path segment; otherwise it's the
 * user's own `config.branch.prefix`. Results are deduped so `origin/X`
 * and local `X` collapse to a single entry (local preferred implicitly
 * — `git branch -a` lists locals before remotes in typical output).
 */
export async function findBranchesForIssue(
  issueLower: string,
  opts: { anyAuthor?: boolean } = {},
): Promise<string[]> {
  const out = await git(["branch", "-a", "--format=%(refname:short)"]).catch(
    () => "",
  );
  // In strict mode we only accept `<michael>/<id>-...`. With anyAuthor
  // we relax to "id appears at a word boundary anywhere in the branch
  // name" — this catches non-standard layouts like
  // `worktree-david+eng-4959-...` that don't use `/` as the separator.
  // The picker modal handles false positives gracefully.
  const pattern = opts.anyAuthor
    ? new RegExp(
        `(?:^|[^a-z0-9])${escapeRegex(issueLower)}(?:-|$)`,
        "i",
      )
    : new RegExp(
        `^(?:origin/)?${escapeRegex(config.branch.prefix)}/${escapeRegex(issueLower)}(?:-|$)`,
      );
  const seen = new Set<string>();
  const branches: string[] = [];
  for (const raw of out.split("\n")) {
    if (!pattern.test(raw)) continue;
    const normalized = raw.replace(/^origin\//, "");
    if (seen.has(normalized)) continue;
    seen.add(normalized);
    branches.push(normalized);
  }
  return branches;
}

function escapeRegex(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

export type ParseInputOptions = {
  slugHint?: string;
  promptForSlug?: (id: string) => Promise<string | null>;
  /**
   * Widen the issue-ID search to branches from any author
   * (`<anyone>/<id>-...`). Without this, only `michael/` matches.
   */
  anyAuthor?: boolean;
  /**
   * Pick one when multiple branches match (e.g. pair-programming
   * across authors). If omitted and there are multiple matches,
   * parseInput throws.
   */
  promptForChoice?: (id: string, branches: string[]) => Promise<string | null>;
};

export async function parseInput(
  raw: string,
  opts: ParseInputOptions = {},
): Promise<string> {
  raw = raw.trim();
  if (!raw) throw new Error("empty input");

  const urlMatch = LINEAR_URL_RE.exec(raw);
  if (urlMatch && urlMatch[1]) raw = urlMatch[1].toUpperCase();

  if (LINEAR_ID_RE.test(raw)) {
    const id = raw.toUpperCase();
    const idLower = id.toLowerCase();
    const found = await findBranchesForIssue(idLower, { anyAuthor: opts.anyAuthor });
    if (found.length === 1) return found[0]!;
    if (found.length > 1) {
      if (opts.promptForChoice) {
        const picked = await opts.promptForChoice(id, found);
        if (!picked) throw new Error(`no branch chosen for ${id}`);
        return picked;
      }
      throw new Error(
        `Multiple branches for ${id}: ${found.join(", ")}. Pass the branch explicitly.`,
      );
    }
    if (opts.slugHint) {
      return `${config.branch.prefix}/${idLower}-${slugify(opts.slugHint)}`;
    }
    if (opts.promptForSlug) {
      const slug = await opts.promptForSlug(id);
      if (!slug) throw new Error(`no slug provided for ${id}`);
      return `${config.branch.prefix}/${idLower}-${slugify(slug)}`;
    }
    throw new Error(`No branch for ${id}; pass slug or run interactively.`);
  }

  if (raw.includes("/")) return raw;
  // Exact-match escape hatch: if the raw input names a real branch
  // (local or origin), attach to it instead of minting a fresh
  // `michael/<slug>`. Covers non-standard names without a `/`
  // separator (e.g. `worktree-david+eng-4959-...`).
  if (await branchExists(raw)) return raw;
  return `${config.branch.prefix}/${slugify(raw)}`;
}

export type CreateOptions = {
  onPhase?: (phase: string) => void;
  onLog?: (line: string) => void;
  runInstall?: boolean; // default true
  /**
   * Base ref for a *new* branch (e.g. `origin/main`, `michael/eng-4999`).
   * Ignored when the branch already exists — in that case we check out
   * the existing branch as-is.
   */
  base?: string;
};

export async function createWorktree(
  branch: string,
  opts: CreateOptions = {},
): Promise<CreateResult> {
  const slug = dirSlug(branch);
  const path = join(config.paths.worktreeRoot, slug);
  const stage = computeStage(slug);

  if (existsSync(path)) {
    return { ok: false, reason: `Path already exists: ${path}` };
  }

  mkdirSync(config.paths.worktreeRoot, { recursive: true });

  const handle = tryAcquireLock(slug, "init", { phase: "preparing" });
  if (!handle) {
    return { ok: false, reason: `Another wt process is busy with ${slug}` };
  }

  // Reset any stale archive / state.json entry left over from a prior
  // destroy of the same slug. Done after lock acquire so a racing
  // destroy of the same slug (would have failed `tryAcquireLock` above)
  // can't have its archive entry wiped from under it. We deliberately
  // don't clean these up at destroy time: clearing archive.json while
  // the parent TUI's worktreesQuery cache still includes the row makes
  // the row "un-archive" mid-destroy and flash back into the active
  // list. Clearing here, paired with the lock guarantee that no
  // destroy is in flight, is the race-free counterpart.
  clearArchived(slug);
  clearSlugState(slug);
  clearRemovedWorktree(slug);
  clearClaudeNames(slug);
  clearCodexNames(slug);
  clearOpencodeNames(slug);

  try {
    opts.onPhase?.("fetching origin");
    await fetchOrigin();

    const backend = getBackend(config.backend.kind);
    handle.phase(`creating worktree (${backend.id})`);
    const existing = await branchExists(branch);
    if (existing && opts.base) {
      opts.onLog?.(`note: --base ignored, ${branch} already exists`);
    }
    // `null` baseRef == "check out the existing branch"; otherwise create
    // a new branch off this ref. The backend materializes the checkout on
    // the branch; wt does the upstream/fork-base wiring below (agnostic —
    // it runs git inside the new checkout, which holds for both a linked
    // worktree and an independent rift clone).
    const baseRef = existing ? null : opts.base ?? `origin/${config.branch.base}`;
    // When the base is a sibling branch (a stacked parent, i.e. not an
    // `origin/` ref), point the backend at that parent's worktree — the
    // rift backend fetches the base commits from it, since an independent
    // clone won't already have them. undefined for trunk/origin bases and
    // ignored by the git-worktree backend.
    let baseSourcePath: string | undefined;
    if (baseRef && !baseRef.startsWith("origin/")) {
      const cand = join(config.paths.worktreeRoot, dirSlug(baseRef));
      if (existsSync(cand)) baseSourcePath = cand;
    }
    await backend.create({
      path,
      branch,
      slug,
      baseRef,
      baseSourcePath,
      mainClone: config.paths.mainClone,
      onLog: opts.onLog,
    });

    if (existing) {
      if (
        (await originBranchExists(branch, path)) &&
        !(await gitQuiet(["rev-parse", "--abbrev-ref", "@{u}"], path))
      ) {
        await gitQuiet(["branch", "--set-upstream-to", `origin/${branch}`], path);
      }
    } else if (baseRef) {
      // Remember a non-trunk fork base. This record IS the stack
      // primitive: it drives the TUI's stack grouping, the diff base,
      // and the restack replay. Stored as a plain branch name so it can
      // match a sibling worktree; the fork-point sha captured now is
      // the squash-safe anchor a later restack replays from.
      const baseBranch = baseRef.replace(/^origin\//, "");
      if (baseBranch !== config.branch.base) {
        const sha = await revParse("HEAD", path);
        setSlugBase(slug, { branch: baseBranch, sha: sha ?? undefined });
        opts.onLog?.(`recorded fork base ${baseBranch}`);
      }
    }

    handle.phase("copying env files");
    for (const name of config.lifecycle.envFilesToCopy) {
      const src = join(config.paths.mainClone, name);
      const dst = join(path, name);
      if (existsSync(src) && !existsSync(dst)) {
        copyFileSync(src, dst);
        opts.onLog?.(`copied ${name}`);
      }
    }

    handle.phase("pinning sst stage");
    const sstDir = join(path, ".sst");
    mkdirSync(sstDir, { recursive: true });
    writeFileSync(join(sstDir, "stage"), `${stage}\n`);
    opts.onLog?.(`pinned sst stage → ${stage}`);

    // The rift backend copies packages across via its CoW clone
    // (`--copy-all`), so wt's own install is redundant — packages are
    // always present without a fresh `pnpm install`, and any lockfile
    // sync is left to rift's `.rift.toml` postcreate hooks. `--no-install`
    // / `runInstall` is therefore a no-op here (ignored, not an error).
    if (backend.id === "rift") {
      opts.onLog?.("packages copied via rift clone (skipping pnpm install)");
    } else if (opts.runInstall !== false) {
      handle.phase("pnpm install");
      opts.onLog?.("pnpm install...");
      const code = await runStreaming(["pnpm", "install"], {
        cwd: path,
        onLine: (line) => opts.onLog?.(line),
      });
      if (code !== 0) {
        throw new Error(`pnpm install exit ${code}`);
      }
    }
  } finally {
    handle.release();
  }

  return { ok: true, path, branch, stage, slug };
}

export type RemoveOptions = {
  force?: boolean;
  destroyStage?: boolean;
  deleteBranch?: boolean;
  onPhase?: (phase: string) => void;
  onLog?: (line: string) => void;
};

export type RemoveResult = {
  ok: boolean;
  message: string;
  destroyedStage: boolean;
  deletedBranch: boolean;
};

/**
 * Foreground remove. Assumes caller already confirmed dirty-prompts
 * and resolved the destroyStage / deleteBranch decisions.
 */
export async function removeWorktree(
  wt: Worktree,
  opts: RemoveOptions = {},
): Promise<RemoveResult> {
  const { force = false, destroyStage = false, deleteBranch = false } = opts;

  // Acquire the per-slug lock, retrying briefly rather than failing on the
  // first miss. A concurrent reader can hold this flock transiently — most
  // notably a restack's `reconcileStack`, which probes each parent's PR
  // state with a LIVE `gh pr view` while holding the whole chain's locks.
  // When an automation cleans a merged member and restacks the survivor in
  // the same dispatch, the detached `wt _destroy` child reaches this line
  // right as reconcile is mid-probe on that member; a single try loses the
  // race and strands the worktree (its clean-fire already consumed). A
  // teardown about to run for seconds shouldn't abort over a sub-second
  // race, so wait out a transient holder. Bounded so a genuinely long-held
  // lock (another destroy, a replay of this very worktree) still fails.
  let handle = tryAcquireLock(wt.slug, "remove", { phase: "preparing" });
  if (!handle) {
    const deadline = Date.now() + LOCK_ACQUIRE_WAIT_MS;
    while (!handle && Date.now() < deadline) {
      await Bun.sleep(150 + Math.floor(Math.random() * 150));
      handle = tryAcquireLock(wt.slug, "remove", { phase: "preparing" });
    }
  }
  if (!handle) {
    const existing = lockStatus(wt.slug);
    return {
      ok: false,
      message: existing
        ? `${wt.slug} is busy: ${lockLabel(existing)}`
        : `could not lock ${wt.slug}`,
      destroyedStage: false,
      deletedBranch: false,
    };
  }

  let effectiveForce = force;
  let destroyedStage = false;
  let deletedBranch = false;
  try {
    if (destroyStage) {
      // Central safety gate. `safe.stage` is the pinned `.sst/stage`,
      // accepted only when it carries the personal prefix — so the
      // destroy targets what's actually deployed but can never point at
      // a foreign (e.g. production) stage outside our namespace.
      const safe = safeStage(wt);
      if (!safe.ok) {
        opts.onPhase?.("sst remove (skipped)");
        handle.phase("sst remove (skipped)");
        opts.onLog?.(`refusing sst remove: ${safe.reason}`);
      } else {
        opts.onPhase?.("sst remove");
        handle.phase("sst remove");
        opts.onLog?.(`pnpm sst remove --stage ${safe.stage}`);
        const sstExit = await runStreaming(
          ["pnpm", "sst", "remove", "--stage", safe.stage],
          {
            cwd: wt.path,
            onLine: (line) => opts.onLog?.(line),
          },
        );
        if (sstExit === 0) {
          destroyedStage = true;
          // sst regenerates tracked files; bypass git's dirty check.
          effectiveForce = true;
        } else {
          opts.onLog?.(`sst remove failed (exit ${sstExit})`);
        }
      }
    }

    // Dispatch on the checkout's ACTUAL backend (derived from disk), not
    // the config's current `kind` — a rift checkout must be torn down
    // with rift even after the user flips the default back to git, and
    // vice versa.
    const backend = getBackendForPath(wt.path);
    opts.onPhase?.(`worktree remove (${backend.id})`);
    handle.phase(`worktree remove (${backend.id})`);
    const removed = await backend.remove({
      path: wt.path,
      slug: wt.slug,
      force: effectiveForce,
      mainClone: config.paths.mainClone,
      onLog: opts.onLog,
    });
    if (!removed.ok) {
      return {
        ok: false,
        message: removed.message ?? "failed",
        destroyedStage,
        deletedBranch,
      };
    }

    if (wt.branch && backend.id === "rift") {
      // A rift branch lives ONLY inside the (now-removed) clone's own
      // `.git`, so it's gone with the checkout unconditionally — there's
      // no shared main-clone ref, and `--keep-branch` can't preserve it
      // (unlike a git-worktree branch, whose ref survives in the shared
      // db). Mark it gone regardless of the deleteBranch flag so
      // dependents always reparent below instead of dangling on a branch
      // that no longer resolves. (The CLI's `decideDeleteBranch` also
      // returns false for a rift branch, since it's not in the main
      // clone — keying on the backend here makes this independent of it.)
      deletedBranch = true;
    } else if (deleteBranch && wt.branch && (await branchExists(wt.branch))) {
      handle.phase("deleting branch");
      if (await gitQuiet(["branch", "-D", wt.branch])) {
        deletedBranch = true;
      }
    }

    // The deleted branch may be some OTHER worktree's recorded fork
    // base. Reparent those records onto the deleted branch's own
    // recorded base (or trunk), PRESERVING their baseSha anchors — the
    // usual reason a parent disappears is that it merged and got
    // cleaned, and the kept anchor is what lets the next restack replay
    // the dependents squash-safely instead of re-applying the landed
    // parent's commits.
    if (deletedBranch && wt.branch) {
      const reparented = reparentBaseReferences(wt.branch, config.branch.base, wt.slug);
      if (reparented.length > 0) {
        opts.onLog?.(
          `reparented fork base on ${reparented.join(", ")} (was the deleted ${wt.branch})`,
        );
      }
    }

    // Note: archive.json / state.json entries for THIS slug are NOT
    // cleared here. Doing so from the child process while the parent
    // TUI's worktreesQuery cache still includes this slug causes the
    // row to visibly "un-archive" mid-destroy: the archive query
    // refetches and sees the slug gone, but the worktrees list still
    // has it, so the row pops into the active section as merged/gone
    // until the next worktrees refetch. The fresh-start guarantee for
    // re-creates lives in createWorktree (which clears both files for
    // the new slug); the stale-entry sweep for external destroys lives
    // in `reapStartup` so archive.json/state.json don't accumulate
    // ghosts. (Dependents' fork-base records were already reparented
    // above when the branch was deleted.)

    // Confirm the removal in the removed-worktrees history. The TUI's
    // destroy flows already wrote a rich snapshot (title, PR) at
    // dispatch; this minimal upsert preserves those fields and covers
    // the CLI paths that never went through the TUI. Best-effort — a
    // state-file IO failure must not fail an already-completed remove.
    if (wt.branch) {
      try {
        recordRemovedWorktrees([
          { slug: wt.slug, branch: wt.branch, removedAt: new Date().toISOString() },
        ]);
      } catch (err) {
        opts.onLog?.(
          `could not record removed-worktree entry: ${err instanceof Error ? err.message : String(err)}`,
        );
      }
    }

    return {
      ok: true,
      message: `removed ${wt.slug}`,
      destroyedStage,
      deletedBranch,
    };
  } finally {
    handle.release();
  }
}

/**
 * Spawn a detached background process to run the destroy tail (including
 * `pnpm sst remove` when `destroyStage` is set).
 *
 * `detached: true` is load-bearing, not cosmetic. Without it the child shares
 * wt's process group + controlling terminal, so closing the terminal window
 * (or an SSH drop) delivers SIGHUP to the whole group and kills an in-flight
 * `sst remove` mid-teardown — a half-removed, stranded stage. setsid (what
 * `detached` triggers) gives the child its own session so the hangup can't
 * reach it; it runs to completion writing into the already-opened log fd.
 * `.unref()` then frees the child from wt's event loop (fire-and-forget; we
 * never await it). A clean wt quit was already survivable (the child reparents
 * to launchd), and a hard kill mid-remove just leaves an orphaned stage that
 * `categorizeStages` re-flags on next launch — the terminal-hangup hole was the
 * one that silently stranded work. This mirrors why actions run under tmux
 * (`core/tmux/action-sessions.ts`): destroy work must outlive the TUI.
 */
export function spawnBackgroundRemove(slug: string, opts: {
  force: boolean;
  destroyStage: boolean;
  deleteBranch: boolean;
}): string {
  mkdirSync(config.paths.logDir, { recursive: true });
  const logPath = join(
    config.paths.logDir,
    `${slug}-${new Date().toISOString().replace(/[:.]/g, "-")}.log`,
  );
  const exe = join(import.meta.dir, "..", "..", "bin", "wt");
  // Open the log file in the parent and pass the fd to the child as
  // stdout+stderr. This captures not only the _destroy process's own
  // writes but also every grandchild (pnpm sst remove, git, etc.)
  // without leaking into the TUI's terminal.
  const fd = openSync(logPath, "a");
  try {
    const child = Bun.spawn(
      [
        exe,
        "_destroy",
        slug,
        "--force",
        String(opts.force),
        "--destroy-stage",
        String(opts.destroyStage),
        "--delete-branch",
        String(opts.deleteBranch),
      ],
      {
        stdin: "ignore",
        stdout: fd,
        stderr: fd,
        // Own session (setsid) so a terminal hangup can't SIGHUP the
        // in-flight `sst remove`. See the docstring above.
        detached: true,
      },
    );
    // Fire-and-forget: don't let the child hold wt's event loop open.
    child.unref();
  } finally {
    // Parent doesn't need the fd — the child has its own dup.
    closeSync(fd);
  }
  return logPath;
}
