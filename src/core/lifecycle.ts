import { closeSync, copyFileSync, existsSync, mkdirSync, openSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { clearArchived } from "./archive.ts";
import { config } from "./config.ts";
import { branchExists, git, gitQuiet } from "./git.ts";
import { LINEAR_ID_RE, LINEAR_URL_RE } from "./linear.ts";
import { lockStatus, tryAcquireLock } from "./locks.ts";
import { run, runQuiet, runStreaming } from "./proc.ts";
import { computeStage, dirSlug, slugify } from "./stage.ts";
import type { Worktree } from "./types.ts";
import { fetchOrigin } from "./worktree.ts";

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

  try {
    opts.onPhase?.("fetching origin");
    await fetchOrigin();

    handle.phase("git worktree add");
    if (await branchExists(branch)) {
      if (opts.base) {
        opts.onLog?.(`note: --base ignored, ${branch} already exists`);
      }
      opts.onLog?.(`checkout ${branch}`);
      await git(["worktree", "add", path, branch]);
      const remoteRef = `refs/remotes/origin/${branch}`;
      if (
        (await gitQuiet(["show-ref", "--verify", "--quiet", remoteRef], path)) &&
        !(await gitQuiet(["rev-parse", "--abbrev-ref", "@{u}"], path))
      ) {
        await gitQuiet(["branch", "--set-upstream-to", `origin/${branch}`], path);
      }
    } else {
      const baseRef = opts.base ?? `origin/${config.branch.base}`;
      opts.onLog?.(`new branch ${branch} off ${baseRef}`);
      await git(["worktree", "add", "--no-track", "-b", branch, path, baseRef]);
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

    if (opts.runInstall !== false) {
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

  const existing = lockStatus(wt.slug);
  if (existing) {
    return {
      ok: false,
      message: `${wt.slug} is busy: ${existing.op ?? "op"} / ${existing.phase ?? "?"}`,
      destroyedStage: false,
      deletedBranch: false,
    };
  }

  const handle = tryAcquireLock(wt.slug, "remove", { phase: "preparing" });
  if (!handle) {
    return {
      ok: false,
      message: `could not lock ${wt.slug}`,
      destroyedStage: false,
      deletedBranch: false,
    };
  }

  let effectiveForce = force;
  let destroyedStage = false;
  let deletedBranch = false;
  try {
    if (destroyStage) {
      opts.onPhase?.("sst remove");
      handle.phase("sst remove");
      opts.onLog?.(`pnpm sst remove --stage ${wt.stage}`);
      const sstExit = await runStreaming(
        ["pnpm", "sst", "remove", "--stage", wt.stage],
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

    opts.onPhase?.("git worktree remove");
    handle.phase("git worktree remove");
    const args = ["worktree", "remove", wt.path];
    if (effectiveForce) args.push("--force");
    const r = await run(["git", ...args], { cwd: config.paths.mainClone });
    if (r.exitCode !== 0) {
      await gitQuiet(["worktree", "prune"]);
      if (existsSync(wt.path)) {
        return {
          ok: false,
          message: (r.stderr || r.stdout || "failed").trim(),
          destroyedStage,
          deletedBranch,
        };
      }
    }

    if (deleteBranch && wt.branch && (await branchExists(wt.branch))) {
      handle.phase("deleting branch");
      if (await gitQuiet(["branch", "-D", wt.branch])) {
        deletedBranch = true;
      }
    }

    // Worktree is fully gone — drop any archived-flag the user set so a
    // future `wt new <same-slug>` doesn't inherit the stale status.
    // Deferred to the end so the row shows its archived styling
    // throughout the destroy instead of flickering non-archived.
    clearArchived(wt.slug);

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

/** Spawn a detached background process to run the destroy tail. */
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
    Bun.spawn(
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
      },
    );
  } finally {
    // Parent doesn't need the fd — the child has its own dup.
    closeSync(fd);
  }
  return logPath;
}
