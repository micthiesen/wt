import {
  closeSync,
  copyFileSync,
  existsSync,
  mkdirSync,
  openSync,
  writeFileSync,
} from "node:fs";
import { dirname, join, normalize } from "node:path";

import { clearArchived } from "./archive.ts";
import { clearClaudeNames } from "./harness/claude/names.ts";
import { clearCodexNames } from "./harness/codex/names.ts";
import { untrustCodexWorkspace } from "./harness/codex/trust.ts";
import { clearOpencodeNames } from "./harness/opencode/names.ts";
import {
  clearRemovedWorktree,
  clearSlugState,
  readWtState,
  recordRemovedWorktrees,
  reparentBaseReferences,
  setSlugBase,
  recordSlugCreated,
} from "./wtstate.ts";
import { getBackend, getBackendForPath } from "./backend.ts";
import { closeWorktreeBrowserSessions } from "./browser.ts";
import { config } from "./config.ts";
import { createLogger } from "./logger.ts";
import { clearDevServerFiles } from "./dev-server.ts";
import { resolveInstallCommand } from "./install.ts";
import {
  branchExists,
  git,
  gitQuiet,
  gitRun,
  originBranchExists,
  revParse,
} from "./git.ts";
import { ISSUE_ID_RE, ISSUE_URL_RE } from "./issue-tracker.ts";
import {
  lockLabel,
  lockStatus,
  tryAcquireLock,
  type LockHandle,
} from "./locks.ts";
import { runStreaming } from "./proc.ts";
import { reapWorktreeListeners } from "./reaper.ts";
import { resolveTeardownCommand, TEARDOWN_TIMEOUT_MS } from "./teardown.ts";
import { RESERVED_SESSION_SLUGS } from "./tmux/naming.ts";
import { computeStage, dirSlug, slugify } from "./stage.ts";
import {
  adjectives,
  animals,
  uniqueNamesGenerator,
} from "unique-names-generator";
import { Cause, Clock, Data, DateTime, Effect, Schedule, Scope } from "effect";
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

const log = createLogger("[lifecycle]");

const causeMessage = (cause: Cause.Cause<unknown>): string => {
  const error = Cause.squash(cause);
  return error instanceof Error ? error.message : String(error);
};

export type CreateResult =
  | { ok: true; path: string; branch: string; stage: string; slug: string }
  | { ok: false; reason: string };

export class LifecycleError extends Data.TaggedError("LifecycleError")<{
  readonly operation: "create" | "remove";
  readonly message: string;
  readonly cause?: unknown;
}> {}


const runDestroyCommandEffect = (opts: {
  command: string;
  cwd: string;
  slug: string;
  onLog?: (line: string) => void;
}): Effect.Effect<boolean> => {
  opts.onLog?.(`destroy_command: ${opts.command}`);
  return runStreaming([process.env.SHELL || "bash", "-lc", opts.command], {
    cwd: opts.cwd,
    onLine: (line) => opts.onLog?.(line),
    killAfterMs: TEARDOWN_TIMEOUT_MS,
  }).pipe(
    Effect.map((exit) => {
      if (exit === 0) return true;
      opts.onLog?.(`destroy_command failed (exit ${exit}) — continuing`);
      log.warn("destroy_command failed", { slug: opts.slug, exit });
      return false;
    }),
    Effect.catch((error) => Effect.sync(() => {
      opts.onLog?.(`destroy_command errored: ${error.message} — continuing`);
      return false;
    })),
  );
};

/**
 * Return branches matching `<prefix>/<issue-id>(-|$)`. When `anyAuthor`
 * is set, `<prefix>` is any single path segment; otherwise it's the
 * user's own `config.branch.prefix`. Results are deduped so `origin/X`
 * and local `X` collapse to a single entry (local preferred implicitly
 * — `git branch -a` lists locals before remotes in typical output).
 */
export const findBranchesForIssue = Effect.fn("findBranchesForIssue")(function* (
  issueLower: string,
  opts: { anyAuthor?: boolean } = {},
): Effect.fn.Return<string[]> {
  const out = yield* git(["branch", "-a", "--format=%(refname:short)"]).pipe(
    Effect.orElseSucceed(() => ""),
  );
  // In strict mode we only accept `<michael>/<id>-...`. With anyAuthor
  // we relax to "id appears at a word boundary anywhere in the branch
  // name". This catches non-standard layouts like
  // `worktree-david+eng-4959-...` that don't use `/` as the separator.
  // The picker modal handles false positives gracefully.
  const pattern = opts.anyAuthor
    ? new RegExp(`(?:^|[^a-z0-9])${escapeRegex(issueLower)}(?:-|$)`, "i")
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
});

function escapeRegex(s: string): string {
  return s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
}

/**
 * Readable random suffix (`cozy-elephant`) for a bare-id `wt new`,
 * retried until the resulting branch is free. ~29k combos; if all five
 * draws collide something is deeply wrong, so give up loudly.
 */
const randomFreeSuffixEffect = Effect.fnUntraced(function* (
  idLower: string,
): Effect.fn.Return<string, ParseInputError> {
  for (let attempt = 0; attempt < 5; attempt++) {
    const suffix = uniqueNamesGenerator({
      dictionaries: [adjectives, animals],
      separator: "-",
      length: 2,
      style: "lowerCase",
    });
    const exists = yield* branchExists(`${config.branch.prefix}/${idLower}-${suffix}`).pipe(
      Effect.mapError((cause) => new ParseInputError({ message: "failed to inspect branches", cause })),
    );
    if (!exists) {
      return suffix;
    }
  }
  return yield* new ParseInputError({
    message: `couldn't find a free random slug for ${idLower} (tried 5)`,
  });
});

export class ParseInputError extends Data.TaggedError("ParseInputError")<{
  readonly message: string;
  readonly cause?: unknown;
}> {}

export type ParseInputOptions<E = never> = {
  slugHint?: string;
  /**
   * Widen the issue-ID search to branches from any author
   * (`<anyone>/<id>-...`). Without this, only `michael/` matches.
   */
  anyAuthor?: boolean;
  /**
   * Attach to an existing branch for the id instead of minting a new
   * one (`--attach`): one match checks out, several go through
   * `promptForChoice`, none is an error. Without it, an id always
   * creates a fresh branch (repeat entries = more worktrees).
   */
  attach?: boolean;
  /**
   * Pick one when multiple branches match (e.g. pair-programming
   * across authors). If omitted and there are multiple matches,
   * parseInput throws.
   *
   * Generic in `E` rather than `unknown`: callers implement this with
   * their own effect (a TUI modal, `pickIndex`'s `PromptError`) and
   * `parseInput` folds whatever it fails with into `ParseInputError`
   * below — the caller's error type is real, just not this module's
   * business, so it's threaded through as a type parameter instead of
   * erased to `unknown`.
   */
  promptForChoice?: (
    id: string,
    branches: string[],
  ) => Effect.Effect<string | null, E> | Promise<string | null>;
};

const parseInputFailure = (
  message: string,
): Effect.Effect<never, ParseInputError> =>
  Effect.fail(new ParseInputError({ message }));

export const parseInput = Effect.fn("parseInput")(function* <E = never>(
  raw: string,
  opts: ParseInputOptions<E> = {},
): Effect.fn.Return<string, ParseInputError> {
    raw = raw.trim();
    if (!raw) return yield* parseInputFailure("empty input");

    // "<ID> [title words…]" is a leading issue id, optionally followed by
    // pasted title text that becomes the slug. There is no tracker API.
    const tokens = raw.split(/\s+/);
    const urlMatch = ISSUE_URL_RE.exec(tokens[0]!);
    if (urlMatch && urlMatch[1]) tokens[0] = urlMatch[1].toUpperCase();
    if (ISSUE_ID_RE.test(tokens[0]!)) {
      const id = tokens[0]!.toUpperCase();
      const idLower = id.toLowerCase();
      if (opts.attach) {
        const found = yield* findBranchesForIssue(idLower, {
          anyAuthor: opts.anyAuthor,
        });
        if (found.length === 1) return found[0]!;
        if (found.length > 1) {
          if (opts.promptForChoice) {
            const pending = opts.promptForChoice(id, found);
            const picked = yield* Effect.isEffect(pending)
              ? pending.pipe(
                  Effect.mapError(
                    (cause) =>
                      new ParseInputError({
                        message: `failed to choose a branch for ${id}`,
                        cause,
                      }),
                  ),
                )
              : Effect.tryPromise({
                  try: () => pending,
                  catch: (cause) =>
                    new ParseInputError({
                      message: `failed to choose a branch for ${id}`,
                      cause,
                    }),
                });
            if (!picked) {
              return yield* parseInputFailure(`no branch chosen for ${id}`);
            }
            return picked;
          }
          return yield* parseInputFailure(
            `Multiple branches for ${id}: ${found.join(", ")}. Pass the branch explicitly.`,
          );
        }
        return yield* parseInputFailure(
          opts.anyAuthor
            ? `No existing branch for ${id} to attach to.`
            : `No ${config.branch.prefix}/ branch for ${id} to attach to. ` +
                `Add --any to search every author's branches.`,
        );
      }
      const required = config.issueTracker?.prefix;
      const prefix = idLower.slice(0, idLower.indexOf("-"));
      if (required && prefix !== required) {
        return yield* parseInputFailure(
          `${id} can't lead a worktree ([issue_tracker] prefix = "${required}"). ` +
            `Use \`wt new ${required.toUpperCase()}-NNNN …${prefix === "gh" ? ` --gh ${id.slice(3)}` : ""}\`, ` +
            `an issue-less slug, or --attach for an existing branch.`,
        );
      }
      const slug = slugify(opts.slugHint ?? tokens.slice(1).join(" "));
      if (slug) {
        return `${config.branch.prefix}/${idLower}-${slug}`;
      }
      return `${config.branch.prefix}/${idLower}-${yield* randomFreeSuffixEffect(idLower)}`;
    }

    // Branch-shaped input (single token with a `/`) passes through as-is.
    if (tokens.length === 1 && raw.includes("/")) return raw;
    // Exact-match escape hatch for non-standard branch names.
    if (
      tokens.length === 1 &&
      (yield* branchExists(raw).pipe(
        Effect.mapError((cause) => new ParseInputError({ message: "failed to inspect branches", cause })),
      ))
    ) {
      return raw;
    }
    // Anything else slugifies into a fresh `<prefix>/<slug>` branch.
    return `${config.branch.prefix}/${slugify(raw)}`;
});

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

const createWorktreeProgram = Effect.fnUntraced(function* (
  branch: string,
  opts: CreateOptions,
  handle: LockHandle,
) {
  const slug = dirSlug(branch);
  const path = join(config.paths.worktreeRoot, slug);
  const stage = computeStage(slug);

  // Slot sessions (wt source, main clone, dotfiles, manager) share the
  // tmux namespace with worktree slugs — a worktree slugged "manager"
  // would receive the fleet coordinator's injections. Refuse up front.
  if (RESERVED_SESSION_SLUGS.includes(slug)) {
    return {
      ok: false,
      reason: `"${slug}" is a reserved session name (${RESERVED_SESSION_SLUGS.join(", ")}) — pick different title words`,
    } as const;
  }

  if (existsSync(path)) {
    return { ok: false, reason: `Path already exists: ${path}` } as const;
  }

  // Reset any stale archive / repository-state entry left over from a prior
  // destroy of the same slug. Done after lock acquire so a racing
  // destroy of the same slug (would have failed `tryAcquireLock` above)
  // can't have its archive entry wiped from under it. We deliberately
  // don't clean these up at destroy time: clearing archive state while
  // the parent TUI's worktreesQuery cache still includes the row makes
  // the row "un-archive" mid-destroy and flash back into the active
  // list. Clearing here, paired with the lock guarantee that no
  // destroy is in flight, is the race-free counterpart.
  yield* Effect.uninterruptible(Effect.sync(() => {
    clearArchived(slug);
    clearSlugState(slug);
    clearRemovedWorktree(slug);
    clearClaudeNames(slug);
    clearCodexNames(slug);
    clearOpencodeNames(slug);
    clearDevServerFiles(slug);
  }));

    const backend = getBackend(config.backend.kind);

    opts.onPhase?.("fetching origin");
    yield* fetchOrigin();

    handle.phase(`creating worktree (${backend.id})`);
    const existing = yield* branchExists(branch);
    if (existing && opts.base) {
      opts.onLog?.(`note: --base ignored, ${branch} already exists`);
    }
    // `null` baseRef == "check out the existing branch"; otherwise create
    // a new branch off this ref. The backend materializes the checkout on
    // the branch; wt does the upstream/fork-base wiring below (agnostic —
    // it runs git inside the new checkout, which holds for both a linked
    // worktree and an independent rift clone).
    const baseRef = existing
      ? null
      : (opts.base ?? `origin/${config.branch.base}`);
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
    const backendInput = {
      path,
      branch,
      slug,
      baseRef,
      baseSourcePath,
      mainClone: config.paths.mainClone,
      onLog: opts.onLog,
    };
    yield* backend.create(backendInput);

    if (existing) {
      if (
        (yield* originBranchExists(branch, path)) &&
        !(yield* gitQuiet(["rev-parse", "--abbrev-ref", "@{u}"], path))
      ) {
        yield* gitQuiet(
          ["branch", "--set-upstream-to", `origin/${branch}`],
          path,
        );
      }
    } else if (baseRef) {
      // Remember the fork base. This record IS the stack primitive: it
      // drives the TUI's stack grouping, the diff base, and the restack
      // replay. Stored as a plain branch name so it can match a sibling
      // worktree; the fork-point sha captured now is the squash-safe
      // anchor a later restack replays from.
      //
      // Recorded for TRUNK forks too, though they are not stacked on
      // anything. The sha is what `forkBaseIsVacuous` measures "has this
      // branch got commits of its own" against, and with no record that
      // guard answers "not vacuous" — i.e. it fails OPEN, on the one
      // population it exists to protect. That left it inert for every
      // unstacked worktree (12 of 13 on the board where this surfaced),
      // and `merged` closes GitHub issues and feeds the clean sweep.
      // Every `baseBranch` reader already normalizes trunk to "no
      // parent" (`stack-layout.ts`, `stack-ops/chain.ts`), so recording
      // it changes no grouping; it just gives the guard its anchor.
      const baseBranch = baseRef.replace(/^origin\//, "");
      const sha = yield* revParse("HEAD", path);
      yield* Effect.uninterruptible(Effect.sync(() =>
        setSlugBase(slug, { branch: baseBranch, sha: sha ?? undefined })));
      if (baseBranch !== config.branch.base) {
        opts.onLog?.(`recorded fork base ${baseBranch}`);
      }
    }

    // Point a bare `gh pr create` at the real merge target. gh resolves
    // its base as: `--base` flag → `branch.<name>.gh-merge-base` config
    // → the REPO DEFAULT BRANCH — and agents are actively steered toward
    // that default (the harness's gitStatus block names it "the branch
    // you will usually use for PRs" in every session). In a repo whose
    // default branch isn't the integration branch, that combination
    // opened PRs against the wrong base: 100-file diffs, red CI on other
    // people's code, review bots confidently auditing someone else's
    // delta. This branch config outranks the bad hint with zero agent
    // cooperation, costs one local config line, and dies with the
    // branch. A new branch records its actual fork base (a stacked
    // branch's PR correctly targets its parent); an existing branch gets
    // the trunk default only when nothing set a value already (it may
    // carry deliberate config from a previous life).
    const ghMergeBase = existing
      ? (yield* gitRun(["config", `branch.${branch}.gh-merge-base`], path))
          .exitCode === 0
        ? null
        : config.branch.base
      : (baseRef ?? "").replace(/^origin\//, "") || config.branch.base;
    if (ghMergeBase) {
      if (
        yield* gitQuiet(
          ["config", `branch.${branch}.gh-merge-base`, ghMergeBase],
          path,
        )
      ) {
        opts.onLog?.(`gh merge base → ${ghMergeBase}`);
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

    if (config.lifecycle.copyGlobs.length > 0) {
      handle.phase("copying configured files");
      const copied = new Set<string>();
      for (const pattern of config.lifecycle.copyGlobs) {
        const glob = new Bun.Glob(pattern);
        for (const relativePath of glob.scanSync({
          cwd: config.paths.mainClone,
          dot: true,
          onlyFiles: true,
        })) {
          const normalizedPath = normalize(relativePath);
          if (/^\.git(?:[\\/]|$)/.test(normalizedPath)) continue;
          if (copied.has(normalizedPath)) continue;
          copied.add(normalizedPath);
          const src = join(config.paths.mainClone, normalizedPath);
          const dst = join(path, normalizedPath);
          if (existsSync(dst)) continue;
          mkdirSync(dirname(dst), { recursive: true });
          copyFileSync(src, dst);
          opts.onLog?.(`copied ${normalizedPath}`);
        }
      }
    }

    // Stage pinning only means something with an SST integration —
    // without [deploy.sst] nothing can ever read the pinned name
    // (`wt stages` refuses, the stage row hides), so skip the write and
    // the log line rather than reporting work that did nothing.
    if (config.sst) {
      handle.phase("pinning sst stage");
      const sstDir = join(path, ".sst");
      mkdirSync(sstDir, { recursive: true });
      writeFileSync(join(sstDir, "stage"), `${stage}\n`);
      opts.onLog?.(`pinned sst stage → ${stage}`);
    }

    // The rift backend copies packages across via its CoW clone
    // (`--copy-all`), so wt's own install is redundant — packages are
    // always present without a fresh install, and any lockfile sync is
    // left to rift's `.rift.toml` postcreate hooks. `--no-install`
    // / `runInstall` is therefore a no-op here (ignored, not an error).
    if (backend.id === "rift") {
      opts.onLog?.("packages copied via rift clone (skipping install)");
    } else if (opts.runInstall !== false) {
      const install = resolveInstallCommand(path);
      if (!install) {
        opts.onLog?.(
          "no lockfile found — skipping install (set [lifecycle] install_command to override)",
        );
      } else {
        handle.phase(install.label);
        opts.onLog?.(`${install.label}...`);
        const code = yield* runStreaming(install.argv, {
          cwd: path,
          onLine: (line) => opts.onLog?.(line),
        });
        if (code !== 0) {
          throw new Error(`${install.label} exit ${code}`);
        }
      }
    }
    yield* Effect.uninterruptible(Effect.sync(() => recordSlugCreated(slug)));
  return { ok: true, path, branch, stage, slug } as const;
});

export const createWorktree = Effect.fn("createWorktree")(function* (
  branch: string,
  opts: CreateOptions = {},
): Effect.fn.Return<Extract<CreateResult, { ok: true }>, LifecycleError, Scope.Scope> {
  const slug = dirSlug(branch);
  const path = join(config.paths.worktreeRoot, slug);
  if (RESERVED_SESSION_SLUGS.includes(slug)) {
    return yield* new LifecycleError({
      operation: "create",
      message: `"${slug}" is a reserved session name (${RESERVED_SESSION_SLUGS.join(", ")}) — pick different title words`,
    });
  }
  if (existsSync(path)) {
    return yield* new LifecycleError({
      operation: "create",
      message: `Path already exists: ${path}`,
    });
  }
  const handle = yield* Effect.acquireRelease(
    Effect.try({
      try: () => {
        mkdirSync(config.paths.worktreeRoot, { recursive: true });
        const acquired = tryAcquireLock(slug, "init", {
          phase: "preparing",
        });
        if (!acquired)
          throw new Error(`Another wt process is busy with ${slug}`);
        return acquired;
      },
      catch: (cause) =>
        new LifecycleError({
          operation: "create",
          message: cause instanceof Error ? cause.message : String(cause),
          cause,
        }),
    }),
    (acquired) => Effect.sync(() => acquired.release()),
  );
  const result = yield* createWorktreeProgram(branch, opts, handle).pipe(
    Effect.catchCause((cause) => {
      const detail = causeMessage(cause);
      log.error(detail, { slug });
      return Effect.fail(new LifecycleError({ operation: "create", message: detail, cause }));
    }),
  );
  if (!result.ok) {
    return yield* new LifecycleError({
      operation: "create",
      message: result.reason,
    });
  }
  return result;
}, Effect.scoped);

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
const removeWorktreeProgram = Effect.fnUntraced(function* (
  wt: Worktree,
  opts: RemoveOptions,
  handle: LockHandle,
) {
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
  let effectiveForce = force;
  let destroyedStage = false;
  let deletedBranch = false;
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
        const sstExit = yield* runStreaming(
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

    // Project teardown BEFORE the reaper, for the same reason the
    // reaper runs before the backend remove: the command runs `cwd` in
    // the checkout, so the directory has to still exist. Graceful first,
    // blunt second — this is the project's own chance to release what it
    // created, and the reaper is the sweep for whatever convention
    // missed. Its blind spot is the whole point: the reaper matches on a
    // listening socket plus a cwd inside the worktree, and a docker
    // container has neither (the host port belongs to the daemon), so
    // nothing about a container is reachable through the process tree.
    const destroyCommand = resolveTeardownCommand(
      config.lifecycle.destroyCommand,
      {
        path: wt.path,
        slug: wt.slug,
        port: readWtState().slugs[wt.slug]?.devPort ?? null,
      },
    );
    if (destroyCommand) {
      opts.onPhase?.("destroy command");
      handle.phase("destroy command");
      const tornDown = yield* runDestroyCommandEffect({
        command: destroyCommand,
        cwd: wt.path,
        slug: wt.slug,
        onLog: opts.onLog,
      });
      // A failed STOP retries on the next stop; a failed DESTROY never
      // retries, because the trigger is the removal that is about to
      // happen. Whatever the command owned is now labelled for a slug
      // with no worktree, no row and no future sweep — the address-pool
      // exhaustion of 2026-08-18 was ~24 of these. Destroying anyway is
      // still right (refusing turns one leak into a bigger one), so the
      // fix is that the leak has to ANNOUNCE itself: `runTeardownCommand`
      // logs file-only, and the destroy log this rides in is read only by
      // someone who already suspects a problem. Attention-level, because
      // the remedy is a command the user has to run.
      if (!tornDown) {
        log.attention.warn(
          `destroy_command failed for ${wt.slug} — anything it owned (containers, ` +
            `networks, volumes) is now orphaned and nothing will sweep it. ` +
            `Check the runtime is up, then re-run the command from wt logs ${wt.slug}.`,
        );
      }
    }

    // Reap hand-started servers (an agent's `pnpm preview`, a stray
    // vite) BEFORE the checkout goes away: lsof resolves each process's
    // cwd against the still-existing directory, and a freed port can't
    // outlive the worktree. This deliberately differs from the browser
    // cleanup below (which waits for the remove to succeed): a killed
    // preview server on a destroy that then bails is one command to
    // restart, while a closed tab's state is gone for good. wt-managed
    // sessions are already dead by now (callers run killAllSessionsFor
    // first), so this only ever sees processes wt doesn't manage.
    const reaped = yield* reapWorktreeListeners(wt.path);
    for (const p of reaped) {
      opts.onLog?.(
        `reaped ${p.command} (pid ${p.pid}, port ${p.ports.join(", ") || "?"})`,
      );
    }

    // Dispatch on the checkout's ACTUAL backend (derived from disk), not
    // the config's current `kind` — a rift checkout must be torn down
    // with rift even after the user flips the default back to git, and
    // vice versa.
    const backend = getBackendForPath(wt.path);
    opts.onPhase?.(`worktree remove (${backend.id})`);
    handle.phase(`worktree remove (${backend.id})`);
    const backendInput = {
      path: wt.path,
      slug: wt.slug,
      force: effectiveForce,
      mainClone: config.paths.mainClone,
      onLog: opts.onLog,
    };
    const removed = yield* backend.remove(backendInput);
    if (!removed.ok) {
      return {
        ok: false,
        message: removed.message ?? "failed",
        destroyedStage,
        deletedBranch,
      };
    }

    // Only now that the checkout is definitely gone — a destroy that
    // bailed above leaves a worktree the user is still working in, and
    // closing its tabs would be pure loss. Best-effort and silent when
    // there was nothing to close, which is the common case.
    const browser = yield* closeWorktreeBrowserSessions(
      wt.slug,
      readWtState().slugs[wt.slug]?.devPort ?? null,
    );
    if (browser.sessions.length > 0) {
      opts.onLog?.(`closed browser session ${browser.sessions.join(", ")}`);
    }
    if (browser.tabs > 0) {
      opts.onLog?.(
        `closed ${browser.tabs} browser tab${browser.tabs === 1 ? "" : "s"}`,
      );
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
    } else if (deleteBranch && wt.branch && (yield* branchExists(wt.branch))) {
      handle.phase("deleting branch");
      if (yield* gitQuiet(["branch", "-D", wt.branch])) {
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
      const reparented = reparentBaseReferences(
        wt.branch,
        config.branch.base,
        wt.slug,
      );
      if (reparented.length > 0) {
        opts.onLog?.(
          `reparented fork base on ${reparented.join(", ")} (was the deleted ${wt.branch})`,
        );
      }
    }

    // A rift checkout may have persisted a Codex trust entry (config.toml, a
    // stowed dotfiles file) when a codex session opened here; drop it so the
    // tracked config doesn't accumulate dead-workspace drift. No-op if none
    // was written. (Claude's ~/.claude.json entry is left — it's Claude's own
    // churny, untracked file, matching the fleet's teardown.)
    if (backend.id === "rift") {
      untrustCodexWorkspace(wt.path);
    }

    // Note: archive / repository-state entries for THIS slug are NOT
    // cleared here. Doing so from the child process while the parent
    // TUI's worktreesQuery cache still includes this slug causes the
    // row to visibly "un-archive" mid-destroy: the archive query
    // refetches and sees the slug gone, but the worktrees list still
    // has it, so the row pops into the active section as merged/gone
    // until the next worktrees refetch. The fresh-start guarantee for
    // re-creates lives in createWorktree (which clears both files for
    // the new slug); the stale-entry sweep for external destroys lives
    // in `reapStartup` so durable state doesn't accumulate
    // ghosts. (Dependents' fork-base records were already reparented
    // above when the branch was deleted.)

    // Confirm the removal in the removed-worktrees history. The TUI's
    // destroy flows already wrote a rich snapshot (title, PR) at
    // dispatch; this minimal upsert preserves those fields and covers
    // the CLI paths that never went through the TUI. Best-effort — a
    // durable-state IO failure must not fail an already-completed remove.
    if (wt.branch) {
      const removedAt = DateTime.formatIso(DateTime.makeUnsafe(yield* Clock.currentTimeMillis));
      yield* Effect.uninterruptible(Effect.try({
        try: () => recordRemovedWorktrees([
          {
            slug: wt.slug,
            branch: wt.branch,
            removedAt,
          },
        ]),
        catch: (cause) => new LifecycleError({
          operation: "remove",
          message: cause instanceof Error ? cause.message : String(cause),
          cause,
        }),
      }).pipe(Effect.catch((err) => Effect.sync(() => opts.onLog?.(
        `could not record removed-worktree entry: ${err instanceof Error ? err.message : String(err)}`,
      )))));
    }

    return {
      ok: true,
      message: `removed ${wt.slug}`,
      destroyedStage,
      deletedBranch,
    };
});

const removeLockSchedule = Schedule.spaced(150).pipe(
  Schedule.jittered,
  Schedule.upTo({ duration: LOCK_ACQUIRE_WAIT_MS }),
);

export const removeWorktree = Effect.fn("removeWorktree")(function* (
  wt: Worktree,
  opts: RemoveOptions = {},
): Effect.fn.Return<RemoveResult, LifecycleError, Scope.Scope> {
  const acquire = Effect.suspend(() => {
    const handle = tryAcquireLock(wt.slug, "remove", { phase: "preparing" });
    return handle
      ? Effect.succeed(handle)
      : Effect.fail(
          new LifecycleError({
            operation: "remove",
            message: `${wt.slug} is busy`,
          }),
        );
  }).pipe(
    Effect.retry({
      schedule: removeLockSchedule,
      while: (error) => error.operation === "remove",
    }),
    Effect.mapError((error) => {
      const existing = lockStatus(wt.slug);
      return new LifecycleError({
        operation: "remove",
        message: existing
          ? `${wt.slug} is busy: ${lockLabel(existing)}`
          : `could not lock ${wt.slug}`,
        cause: error,
      });
    }),
  );

  const handle = yield* Effect.acquireRelease(acquire, (acquired) =>
    Effect.sync(() => acquired.release()),
  );
  const result = yield* removeWorktreeProgram(wt, opts, handle).pipe(
    Effect.catchCause((cause) => Effect.fail(new LifecycleError({
      operation: "remove",
      message: causeMessage(cause),
      cause,
    }))),
  );
  if (!result.ok) {
    return yield* new LifecycleError({
      operation: "remove",
      message: result.message,
    });
  }
  return result;
}, Effect.scoped);

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
export const spawnBackgroundRemove = Effect.fn("spawnBackgroundRemove")(function* (
  slug: string,
  opts: {
    force: boolean;
    destroyStage: boolean;
    deleteBranch: boolean;
  },
): Effect.fn.Return<string, LifecycleError> {
  const stamp = DateTime.formatIso(DateTime.makeUnsafe(yield* Clock.currentTimeMillis)).replace(/[:.]/g, "-");
  const logPath = join(config.paths.logDir, `${slug}-${stamp}.log`);
  const exe = join(import.meta.dir, "..", "..", "bin", "wt");
  // Open the log file in the parent and pass the fd to the child as
  // stdout+stderr. This captures not only the _destroy process's own
  // writes but also every grandchild (pnpm sst remove, git, etc.)
  // without leaking into the TUI's terminal.
  const fail = (cause: unknown) => new LifecycleError({
    operation: "remove",
    message: cause instanceof Error ? cause.message : String(cause),
    cause,
  });
    yield* Effect.try({
      try: () => mkdirSync(config.paths.logDir, { recursive: true }),
      catch: fail,
    });
    return yield* Effect.acquireUseRelease(
      Effect.try({ try: () => openSync(logPath, "a"), catch: fail }),
      (fd) => Effect.try({
        try: () => {
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
          return logPath;
        },
        catch: fail,
      }),
      // Parent doesn't need the fd; the child has its own dup.
      (fd) => Effect.sync(() => closeSync(fd)),
    );
});
