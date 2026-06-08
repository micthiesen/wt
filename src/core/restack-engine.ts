/**
 * Thin seam around the squash-safe restack ENGINE. wt owns the stack
 * manifest (the truth); the engine is a driven dependency that does the
 * one genuinely hard thing — squash-safe restack (record a merge-base
 * anchor, cherry-pick-replay the commit range onto the new parent,
 * force-with-lease push, retarget the PR base) and merge-queue landing.
 *
 * Everything callers need is expressed through `RestackEngine` so the
 * engine can later be reimplemented inside wt and the `@kitlangton/stack`
 * dependency dropped without touching any call site. The default impl
 * (`StackCliEngine`) shells out to the `stack` CLI.
 *
 * Concurrency: the engine's `.git/stack/state.json` resolves via
 * `git rev-parse --git-common-dir`, so it is SHARED across every
 * worktree of this repo and is NOT safe under concurrent syncs. The CLI
 * engine serialises mutating calls behind a cross-process flock
 * (`STACK_LOCK_SLUG`); a caller that can't get the lock gets a clean
 * `{ ok: false, conflict: false }` with an explanatory message rather
 * than racing.
 */
import { config } from "./config.ts";
import { tryAcquireLock } from "./locks.ts";
import { createLogger } from "./logger.ts";
import { run } from "./proc.ts";

const log = createLogger("[restack]");

/** Flock slug guarding the shared `.git/stack` state from concurrent mutation. */
export const STACK_LOCK_SLUG = "__stack__";

const STACK_BIN = "stack";

export type EngineResult = {
  ok: boolean;
  stdout: string;
  stderr: string;
  exitCode: number;
  /**
   * True when the engine bailed on a cherry-pick conflict (aborted
   * clean, left a backup branch, wrote its undo journal). The caller
   * should NOT retry — it hands off to a human / skill to resolve.
   */
  conflict: boolean;
  /** Branch the engine was repairing when it bailed, parsed from output. */
  failedBranch?: string;
  /** `backup/...` branch the engine left behind on a bail, parsed from output. */
  backupBranch?: string;
};

export type SyncOptions = { apply?: boolean };
export type MergeOptions = {
  apply?: boolean;
  auto?: boolean;
  /** `--through <branch-or-change>`: land one root at a time up to here. */
  through?: string;
};

export interface RestackEngine {
  /** Record stack intent: `branch` stacks onto `parent`. Pure metadata. */
  track(branch: string, parent: string): Promise<EngineResult>;
  /** Reconcile + repair drift. `apply` mutates; otherwise it's a preview. */
  sync(branch: string | undefined, opts?: SyncOptions): Promise<EngineResult>;
  /** Land the stack root (and repair descendants). */
  merge(branch: string | undefined, opts?: MergeOptions): Promise<EngineResult>;
  /** Inspect the engine's view of the tracked stack graph. */
  status(branch?: string): Promise<EngineResult>;
}

/** Pull a `backup/...` branch name out of engine output, if present. */
function parseBackupBranch(text: string): string | undefined {
  const m = text.match(/\bbackup\/[^\s'"]+/);
  return m ? m[0] : undefined;
}

/**
 * A cherry-pick replay conflict is the engine's clean-bail signal: it
 * aborts mid-rebase, restores the branch, leaves a backup, and exits
 * nonzero with guidance. Detect it by the backup branch + conflict
 * vocabulary so callers can hand off instead of retrying.
 */
function looksLikeConflict(text: string): boolean {
  return /conflict|could not apply|cherry-pick|failed to (?:replay|rebase)/i.test(
    text,
  );
}

/** Connectives the loose "failed to …" pattern wrongly grabs as a branch. */
const NOT_A_BRANCH = /^(?:onto|into|from|to|the|a|on|in|main|master|trunk)$/i;

/** Branch named in a "failed to … <branch>" / "repair <branch>" line. */
function parseFailedBranch(text: string): string | undefined {
  // The engine narrates each replay as `rebase <branch> onto <base>`;
  // the last one before it bails names the branch it was on. Prefer that
  // over the generic "failed to …" phrase, whose object is often the
  // connective ("onto") rather than the branch — which surfaced as the
  // nonsense "failing branch: onto".
  const steps = [...text.matchAll(/\brebase\s+(\S+)\s+onto\b/gi)];
  const fromStep = steps.at(-1)?.[1];
  if (fromStep && !NOT_A_BRANCH.test(fromStep)) return fromStep;
  const m = text.match(
    /(?:repair|failed (?:to [a-z]+|on)|conflict (?:on|in))\s+([^\s'".,]+)/i,
  );
  return m && !NOT_A_BRANCH.test(m[1]!) ? m[1] : undefined;
}

export class StackCliEngine implements RestackEngine {
  /** Engine state is per-common-dir; always drive from the main clone. */
  private readonly cwd = config.paths.mainClone;

  /** Run a non-mutating engine command (no lock needed). */
  private async readonlyRun(args: string[]): Promise<EngineResult> {
    return this.exec(args, { mutating: false });
  }

  /**
   * Run a mutating engine command behind the cross-process flock so two
   * syncs can't race the shared `.git/stack` state. Track is metadata-
   * only but still touches state.json, so it locks too.
   */
  private async mutatingRun(args: string[]): Promise<EngineResult> {
    return this.exec(args, { mutating: true });
  }

  private async exec(
    args: string[],
    opts: { mutating: boolean },
  ): Promise<EngineResult> {
    const handle = opts.mutating
      ? tryAcquireLock(STACK_LOCK_SLUG, "stack", { phase: args[0] ?? "stack" })
      : null;
    if (opts.mutating && !handle) {
      return {
        ok: false,
        stdout: "",
        stderr:
          "another wt stack operation is already running (shared .git/stack state is not concurrency-safe)",
        exitCode: -1,
        conflict: false,
      };
    }
    try {
      log.debug("running engine", { args });
      const r = await run([STACK_BIN, ...args], { cwd: this.cwd });
      const combined = `${r.stdout}\n${r.stderr}`;
      const conflict = r.exitCode !== 0 && looksLikeConflict(combined);
      return {
        ok: r.exitCode === 0,
        stdout: r.stdout,
        stderr: r.stderr,
        exitCode: r.exitCode,
        conflict,
        ...(conflict ? { failedBranch: parseFailedBranch(combined) } : {}),
        ...(conflict ? { backupBranch: parseBackupBranch(combined) } : {}),
      };
    } finally {
      handle?.release();
    }
  }

  track(branch: string, parent: string): Promise<EngineResult> {
    return this.mutatingRun(["track", branch, "--onto", parent]);
  }

  sync(branch: string | undefined, opts: SyncOptions = {}): Promise<EngineResult> {
    const args = ["sync"];
    // `stack sync` mutates by default; `--dry-run` makes it a pure
    // preview. (Unlike `merge`, it has no `--apply` flag.) So the apply
    // path runs bare and takes the lock; the preview path adds --dry-run.
    if (!opts.apply) args.push("--dry-run");
    if (branch) args.push(branch);
    return opts.apply ? this.mutatingRun(args) : this.readonlyRun(args);
  }

  merge(
    branch: string | undefined,
    opts: MergeOptions = {},
  ): Promise<EngineResult> {
    const args = ["merge"];
    if (branch) args.push(branch);
    if (opts.auto) args.push("--auto");
    else if (opts.apply) args.push("--apply");
    if (opts.through) args.push("--through", opts.through);
    // `merge` with no --apply/--auto is a dry run; both mutate.
    return opts.apply || opts.auto ? this.mutatingRun(args) : this.readonlyRun(args);
  }

  status(branch?: string): Promise<EngineResult> {
    const args = ["status"];
    if (branch) args.push(branch);
    return this.readonlyRun(args);
  }
}

/** Default engine instance — swap the constructor here to drop the dep. */
export const restackEngine: RestackEngine = new StackCliEngine();
