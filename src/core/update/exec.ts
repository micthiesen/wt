/**
 * Process/git plumbing for the self-update system, plus the version
 * string. EVERYTHING under `core/update/` must stay config-free: the
 * crash-rollback path (main.ts catch → cli/commands/rollback.ts) has
 * to work when `core/config.ts` is exactly what the broken update
 * can't load — so no imports of config, proc, locks, or logger (all of
 * which pull the config chain in at module init). `runIn` is a local
 * copy of proc.ts's `run` minus config defaults and signal plumbing;
 * `logSafe` is a best-effort lazy logger that silently no-ops when the
 * logging chain itself can't load.
 */
import { appendFileSync, mkdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import { constants as osConstants, homedir } from "node:os";
import { join, resolve } from "node:path";
import { Clock, Data, Duration, Effect, Option, Schedule, Scope } from "effect";

import { causeMessage } from "../errors.ts";

/** Repo root of the wt source tree (this file is `<root>/src/core/update/exec.ts`). */
export const WT_REPO_ROOT: string = resolve(import.meta.dir, "..", "..", "..");

export type RunResult = { stdout: string; stderr: string; exitCode: number };

export class UpdateProcessError extends Data.TaggedError("UpdateProcessError")<{
  readonly argv: readonly string[];
  readonly operation: "spawn" | "read" | "timeout";
  readonly cause?: unknown;
}> {
  override get message(): string {
    const argv = this.argv.join(" ");
    return this.operation === "timeout"
      ? `timed out: ${argv}`
      : `${this.operation} failed for "${argv}": ${causeMessage(this.cause)}`;
  }
}

function killUpdateProcessGroup(
  proc: Bun.Subprocess<"ignore", "pipe", "pipe">,
): void {
  try {
    process.kill(-proc.pid, "SIGKILL");
  } catch {
    if (proc.exitCode === null) proc.kill("SIGKILL");
  }
}

const processFailureResult = (error: UpdateProcessError): RunResult => ({
  stdout: "",
  stderr:
    error.operation === "timeout"
      ? `timed out: ${error.argv.join(" ")}`
      : error.cause instanceof Error
        ? error.cause.message
        : String(error.cause ?? error.operation),
  exitCode: error.operation === "timeout" ? -2 : -1,
});

/** Config-free, scoped process runner used by the updater and rollback path. */
export function runIn(
  argv: string[],
  opts: { cwd: string; timeoutMs?: number },
): Effect.Effect<RunResult, UpdateProcessError> {
  return Effect.acquireUseRelease(
    Effect.try({
      try: () => {
        const proc = Bun.spawn(argv, {
          cwd: opts.cwd,
          stdin: "ignore",
          stdout: "pipe",
          stderr: "pipe",
          detached: true,
        });
        return {
          proc,
          stdout: new Response(proc.stdout).text(),
          stderr: new Response(proc.stderr).text(),
          exited: proc.exited,
        };
      },
      catch: (cause) =>
        new UpdateProcessError({ argv, operation: "spawn", cause }),
    }),
    (running) => {
      const capture = Effect.tryPromise({
        try: async () => {
          const [stdout, stderr, exitCode] = await Promise.all([
            running.stdout,
            running.stderr,
            running.exited,
          ]);
          return { stdout, stderr, exitCode };
        },
        catch: (cause) =>
          new UpdateProcessError({ argv, operation: "read", cause }),
      });
      if (!opts.timeoutMs) return capture;
      return capture.pipe(
        Effect.timeoutOption(Duration.millis(opts.timeoutMs)),
        Effect.flatMap(
          Option.match({
            onSome: Effect.succeed,
            onNone: () =>
              Effect.andThen(
                Effect.sync(() => killUpdateProcessGroup(running.proc)),
                Effect.fail(
                  new UpdateProcessError({ argv, operation: "timeout" }),
                ),
              ),
          }),
        ),
      );
    },
    (running) =>
      Effect.promise(async () => {
        if (running.proc.exitCode === null) {
          killUpdateProcessGroup(running.proc);
        }
        await Promise.allSettled([
          running.stdout,
          running.stderr,
          running.exited,
        ]);
      }),
  );
}

/** Non-failing process result for command flows where exitCode is the contract. */
export function runInResult(
  argv: string[],
  opts: { cwd: string; timeoutMs?: number },
): Effect.Effect<RunResult> {
  return runIn(argv, opts).pipe(
    Effect.catch((error) => Effect.succeed(processFailureResult(error))),
  );
}

export function gitOk(
  args: string[],
  timeoutMs = 10_000,
): Effect.Effect<string | null, never> {
  return runIn(["git", ...args], {
    cwd: WT_REPO_ROOT,
    timeoutMs,
  }).pipe(
    Effect.map((result) =>
      result.exitCode === 0 ? result.stdout.trim() : null,
    ),
    Effect.orElseSucceed(() => null),
  );
}

export function gitSync(args: string[]): string | null {
  try {
    // Bounded: this runs on the unconditional interactive boot path
    // (sentinel arm), where a hung git must not hang every launch.
    const r = Bun.spawnSync(["git", "-C", WT_REPO_ROOT, ...args], {
      stdout: "pipe",
      stderr: "ignore",
      timeout: 10_000,
    });
    return r.exitCode === 0 ? r.stdout.toString().trim() : null;
  } catch {
    return null;
  }
}

const UPDATE_LOG_FILE = join(homedir(), ".cache", "wt", "logs", "update.log");

/**
 * Best-effort file logging to a FIXED path, deliberately not the app
 * logger: `core/logger.ts` loads the user config at module init, and
 * in the crash path the config loader may be the very thing that's
 * broken — worse, it `process.exit(1)`s on a bad config, which would
 * kill the rollback offer from a fire-and-forget import. Losing a log
 * line is always preferable to that.
 */
export function logSafe(level: "warn" | "error", msg: string): void {
  try {
    mkdirSync(join(homedir(), ".cache", "wt", "logs"), { recursive: true });
    appendFileSync(UPDATE_LOG_FILE, `${new Date().toISOString()} ${level.toUpperCase()} [update] ${msg}\n`);
  } catch {
    // Nothing — logging must never take down an update/rollback path.
  }
}

/**
 * Replace this process's role with a fresh wt: spawn `src/main.ts`
 * with inherited stdio and block until it exits. Used after an
 * accepted update or rollback, where the current process's loaded
 * modules are stale. Signal deaths map to the conventional 128+N so a
 * Ctrl-C in the child doesn't read as a generic failure.
 */
export function spawnFreshWt(): number {
  const child = Bun.spawnSync({
    cmd: [process.execPath, join(WT_REPO_ROOT, "src", "main.ts")],
    stdin: "inherit",
    stdout: "inherit",
    stderr: "inherit",
  });
  if (typeof child.exitCode === "number") return child.exitCode;
  const signum = child.signalCode
    ? osConstants.signals[child.signalCode as keyof typeof osConstants.signals]
    : undefined;
  return signum ? 128 + signum : 1;
}

/**
 * Cross-process mutual exclusion for the destructive git operations
 * (merge/reset on the ONE shared source clone). `core/locks.ts` is
 * off-limits here (config chain + FFI), so this is a plain mkdir
 * lock: atomic on every filesystem, config-free, and a dead holder is
 * detected by pid-liveness (EPERM still means alive) or age. Waits
 * briefly rather than long — the holder may legitimately run `bun
 * install` for minutes, and "another update is in progress, retry" is
 * a better answer than a silent multi-minute block.
 */
const GIT_LOCK_DIR = join(homedir(), ".cache", "wt", "update-git.lock");

class UpdateLockBusy extends Data.TaggedError("UpdateLockBusy") {
  override get message(): string {
    return "update-git lock is held by another process";
  }
}

const releaseUpdateGitLockAt = (lockDir: string) => (): void => {
  try {
    rmSync(lockDir, { recursive: true, force: true });
  } catch {
    // Staleness detection reclaims a lock whose release was interrupted.
  }
};

function pidIsStale(lockDir: string, now: number): boolean {
  const pid = parseInt(readFileSync(join(lockDir, "pid"), "utf8"), 10);
  let alive = false;
  if (Number.isFinite(pid) && pid > 0) {
    try {
      process.kill(pid, 0);
      alive = true;
    } catch (error) {
      alive = (error as NodeJS.ErrnoException).code === "EPERM";
    }
  }
  return !alive || now - statSync(lockDir).mtimeMs > 15 * 60_000;
}

function mtimeOnlyIsStale(lockDir: string, now: number): boolean {
  return now - statSync(lockDir).mtimeMs > 60_000;
}

const acquireUpdateGitLockOnceAt = (lockDir: string) =>
  Effect.fnUntraced(function* (): Effect.fn.Return<() => void, UpdateLockBusy> {
    const release = releaseUpdateGitLockAt(lockDir);
    const acquired = yield* Effect.try(() => {
      mkdirSync(lockDir, { recursive: false });
      writeFileSync(join(lockDir, "pid"), String(process.pid));
    }).pipe(Effect.as(true), Effect.orElseSucceed(() => false));
    if (acquired) return release;

    const now = yield* Clock.currentTimeMillis;
    const stale = yield* Effect.try(() => pidIsStale(lockDir, now)).pipe(
      Effect.catch(() => Effect.try(() => mtimeOnlyIsStale(lockDir, now))),
      Effect.orElseSucceed(() => false),
    );
    if (stale) release();
    return yield* new UpdateLockBusy();
  });

/**
 * Lock-directory-parameterized form, so tests can point at a throwaway
 * directory instead of the real `~/.cache/wt/update-git.lock` a live wt
 * instance on the same machine may be holding. Production goes through
 * the fixed-path `acquireUpdateGitLock` below.
 */
export function acquireUpdateGitLockAt(lockDir: string): Effect.Effect<(() => void) | null> {
  return acquireUpdateGitLockOnceAt(lockDir)().pipe(
    Effect.retry(
      Schedule.max([
        Schedule.spaced(Duration.millis(200)),
        Schedule.recurs(9),
      ]),
    ),
    Effect.orElseSucceed(() => null),
  );
}

export const acquireUpdateGitLock: Effect.Effect<(() => void) | null> = acquireUpdateGitLockAt(GIT_LOCK_DIR);

/** Scoped lock ownership. Scope close releases on success, failure, or interruption. */
export function updateGitLockAt(lockDir: string): Effect.Effect<boolean, never, Scope.Scope> {
  return Effect.acquireRelease(
    acquireUpdateGitLockAt(lockDir),
    (release) => release ? Effect.sync(release) : Effect.void,
  ).pipe(Effect.map((release) => release !== null));
}

export const updateGitLock: Effect.Effect<boolean, never, Scope.Scope> = updateGitLockAt(GIT_LOCK_DIR);

/** Display form of a full sha. */
export function shortSha(sha: string): string {
  return sha.slice(0, 7);
}

// ── Version ────────────────────────────────────────────────────────────

let _version: string | null = null;

/**
 * The running version: `<short-sha> (<commit-date>)`, with a `-dirty`
 * suffix on the sha when the clone has local modifications. Sync and
 * cached for the process lifetime — callers are the help overlay title
 * and `wt version`, both fine paying one ~5ms git call once. The cache
 * is reset by apply/rollback so a post-mutation read shows the new HEAD.
 */
export function wtVersion(): string {
  if (_version !== null) return _version;
  // Remote packages have no Git metadata. The installer records the build.
  try {
    const runtime = JSON.parse(readFileSync(join(WT_REPO_ROOT, ".wt-runtime.json"), "utf8"));
    if (typeof runtime.build === "string" && /^[a-f0-9]{64}$/.test(runtime.hash)) {
      return (_version = runtime.build);
    }
  } catch {
    // Source checkouts use Git below.
  }
  const head = gitSync(["log", "-1", "--format=%h %cs"]);
  if (!head) return (_version = "unknown");
  const [sha, date] = head.split(" ");
  const dirty = gitSync(["status", "--porcelain"]);
  _version = `${sha}${dirty ? "-dirty" : ""}${date ? ` (${date})` : ""}`;
  return _version;
}

export function resetWtVersionCache(): void {
  _version = null;
}
