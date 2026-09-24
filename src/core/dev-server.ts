/**
 * Per-worktree dev server (`[dev_server]`) — start/stop/status for one
 * supervised long-running process per worktree.
 *
 * Process model: the command runs inside a tmux session on the
 * wt-private server (`<slug>-dev`), under a small POSIX-sh supervisor
 * loop written to `~/.cache/wt/dev/<slug>.sh`. That buys the three
 * properties the feature needs for free:
 *
 *  - survives wt restarts (tmux outlives the TUI; wt just observes);
 *  - restarts on crash without thrashing — an exit within
 *    `RAPID_CRASH_SECONDS` of start counts as a rapid failure, and
 *    `GIVE_UP_AFTER` consecutive rapid failures parks the supervisor
 *    (state `crashed`, pane kept via remain-on-exit so the logs stay
 *    readable); a SIGINT/SIGTERM exit is treated as an intentional stop;
 *  - cleaned up with the worktree — the `-dev` suffix is a registered
 *    session kind, so `killAllSessionsFor` (destroy) and the startup
 *    orphan reaper sweep it like every other kind.
 *
 * Port ownership: wt allocates each slug a stable port from
 * `[port_base, port_base + port_range)`, persisted in wtstate
 * (`devPort`, freed with `clearSlugState`). The command template pins
 * the server to it via `{{port}}` — auto-picking servers (vite's
 * port-increment) are exactly what this exists to avoid, since a
 * drifted port breaks recorded URLs and hardcoded HMR sockets.
 *
 * The supervisor writes a one-word marker file
 * (`~/.cache/wt/dev/<slug>.state`: running | stopped | crashed) that
 * `devServerStatus` combines with "does the session exist" and "is the
 * port accepting connections" into the level-derived state the row and
 * bolt render.
 */
import { mkdirSync, readdirSync, readFileSync, rmSync, statSync, writeFileSync } from "node:fs";
import net from "node:net";
import { join } from "node:path";
import { Clock, Data, Duration, Effect } from "effect";

import { closeDevServerBrowserSessions } from "./browser.ts";
import { config, type DevServerConfig } from "./config.ts";
import { causeMessage } from "./errors.ts";
import { createLogger } from "./logger.ts";
import { run, sanitizeLine } from "./proc.ts";
import { resolveTeardownCommand, runTeardownCommand } from "./teardown.ts";
import { sessionName, shQuote, SUFFIX, TMUX_SOCKET } from "./tmux/naming.ts";
import { capturePane, killByName, paneTarget, probeSessionNames } from "./tmux/process.ts";
import { revParse, shaIsAncestor } from "./git.ts";
import { listWorktrees } from "./worktree.ts";
import {
  claimDevPort,
  readWtState,
  setSlugDevStartedSha,
  WT_STATE_DIR,
} from "./wtstate.ts";

const log = createLogger("[dev-server]");

export class DevServerOperationError extends Data.TaggedError("DevServerOperationError")<{
  readonly slug: string;
  readonly operation: "slot" | "status" | "health" | "port" | "start";
  readonly cause: unknown;
}> {
  override get message(): string {
    return `${this.operation} (${this.slug}): ${causeMessage(this.cause)}`;
  }
}

const DEV_DIR = join(WT_STATE_DIR, "dev");
/**
 * A run shorter than this never established itself, so its exit counts
 * toward giving up. Longer runs reset the counter — the server was
 * doing its job and something else killed it.
 *
 * It was 10 seconds, and that made the give-up UNREACHABLE for exactly
 * the projects where looping costs the most. A command that brings up a
 * container stack and then fails on a migration takes far longer than
 * ten seconds to get there, so every attempt looked like a long healthy
 * run, the counter reset every pass, and the supervisor restarted a
 * deterministically-broken twelve-container stack forever. A fast-
 * failing `vite` parked in six seconds; a slow-failing stack never
 * parked at all. The guard failed open on the expensive half.
 *
 * Five minutes because the question is "did this ever serve anything",
 * not "did it exit quickly": a dev server that dies within minutes of
 * every start is broken however long each attempt took.
 */
const ESTABLISHED_AFTER_SECONDS = 300;
/** Consecutive un-established runs before the supervisor parks. */
const GIVE_UP_AFTER = 3;
/** First wait between restarts; doubles up to `MAX_RESTART_DELAY`. */
const RESTART_DELAY_SECONDS = 2;
/**
 * Ceiling on the backoff. Belt and braces next to the give-up above —
 * if a project ever finds another way past it, the loop still costs
 * one attempt a minute rather than one every two seconds. A
 * crash-looping stack is not merely untidy: it is a direct contributor
 * to the fleet saturation that makes unrelated test suites fail and
 * look like real bugs.
 */
const MAX_RESTART_DELAY_SECONDS = 60;
/**
 * A failure faster than this, repeated with an identical exit code AND
 * an identical last output line, is treated as deterministic and parks
 * the supervisor immediately instead of spending the remaining
 * attempts.
 *
 * The case it exists for is a project GUARD, not a crash: an allocator
 * that refuses a port-base disagreement, a preflight that finds the
 * wrong schema. Those print the cause and the fix, exit in under a
 * second, and will do it identically forever, so the retries are pure
 * latency in front of an answer that is already correct and complete.
 *
 * Both halves are required on purpose. The exit code alone is far too
 * common to mean anything (everything fails with 1), and a flaky bind
 * that genuinely deserves its retries can repeat one. Two consecutive
 * failures agreeing on the code *and* the final line is a much
 * stronger claim, and the cost of being wrong is only that a real
 * flake parks one attempt early and the human restarts it.
 */
const DETERMINISTIC_FAILURE_SECONDS = 10;

export type DevServerStatus = {
  /** Session exists and the recorded port accepts connections. */
  running: boolean;
  /** Session exists, port not (yet) accepting, supervisor not parked. */
  starting: boolean;
  /** Supervisor gave up after repeated rapid crashes; logs kept in the dead pane. */
  crashed: boolean;
  /** The slug's recorded port, if one was ever allocated. */
  port: number | null;
  /** Resolved URL when running, else null. */
  url: string | null;
  /**
   * When the current attempt began (the supervisor rewrites its marker
   * at the top of every loop iteration), or null when it never ran.
   *
   * Exists so `starting` can say how long it has been starting. A stack
   * that needs docker can legitimately take minutes, and a hung one is
   * indistinguishable from a slow one without the clock — one worktree
   * sat at `starting` for fifteen minutes with nothing on any surface
   * to say whether that was normal.
   */
  since: number | null;
  /**
   * Set while this slug is queued for a dev slot (`wt dev start
   * --wait`): its position in the fleet-wide queue and when it joined.
   * The row renders it, so an agent blocked on the cap is visible on
   * the board rather than looking idle.
   */
  waiting: { rank: number; since: number } | null;
  /**
   * The running server came up on a commit that is no longer an
   * ancestor of HEAD — history was rewritten underneath it (a rebase,
   * a reset, a restack). Null when it can't be told: no recorded
   * anchor, no path to ask git with, or nothing running.
   *
   * Deliberately NOT "HEAD moved". Ordinary commits keep the anchor an
   * ancestor and a hot-reloading server absorbs them; flagging those
   * would fire on every commit, and a warning that fires constantly is
   * one people learn to scroll past. Rewritten history is the case
   * where a dev environment holding anything derived from the tree — a
   * migrated database above all — is silently describing a tree that
   * no longer exists.
   */
  rebasedSince: boolean | null;
  /**
   * Consecutive failed starts the supervisor has counted, and the last
   * exit code. Zero/absent while a first attempt is still in flight.
   *
   * Without this a restart loop is indistinguishable from a slow start:
   * both render `starting`, and a stack that takes minutes to come up
   * looks exactly like one failing every ninety seconds. "Nothing
   * distinguishes running from restarting for the four-hundredth time"
   * was the complaint, and it is the same complaint as a green test on
   * a saturated box — the surface reported a state without reporting
   * which world produced it.
   */
  restarts: { count: number; lastExit: number } | null;
};

export const DEV_SERVER_STOPPED: DevServerStatus = {
  running: false,
  starting: false,
  crashed: false,
  port: null,
  url: null,
  since: null,
  waiting: null,
  rebasedSince: null,
  restarts: null,
};

/** Maximum app-output characters carried on one attention-feed line. */
const CRASH_SUMMARY_CHARS = 180;

/**
 * Turn a supervisor pane snapshot into one useful, feed-safe error line.
 * The supervisor's own `wt:` epilogue says only that retries stopped;
 * the last application line before it is the part that usually says why.
 */
export function devServerCrashSummary(output: string): string | null {
  const lines = output
    .split("\n")
    .map((line) => sanitizeLine(line).replace(/\s+/g, " ").trim())
    .filter((line) => line.length > 0 && !line.startsWith("wt:"));
  const summary = lines.at(-1);
  if (!summary) return null;
  return summary.length > CRASH_SUMMARY_CHARS
    ? `${summary.slice(0, CRASH_SUMMARY_CHARS - 1)}…`
    : summary;
}

/** Recent output retained in the dev supervisor's active (or parked) pane. */
export function devServerLogs(slug: string, lines = 200) {
 return run([
    "tmux",
    "-L",
    TMUX_SOCKET,
    "capture-pane",
    "-p",
    "-t",
    `=${sessionName(slug, "dev")}:`,
    "-S",
    `-${lines}`,
  ]).pipe(Effect.map((r) => r.exitCode === 0 ? r.stdout.trimEnd() : null));
}

function requireDevServer(): DevServerConfig {
  if (!config.devServer) {
    throw new Error("[dev_server] is not configured in config.toml");
  }
  return config.devServer;
}

function markerPath(slug: string): string {
  return join(DEV_DIR, `${slug}.state`);
}

/**
 * The supervisor's `<count> <exit>` line, written after every failed
 * run. Absent until something fails, which is the common case.
 */
function readAttempts(slug: string): { count: number; lastExit: number } | null {
  try {
    const [c, e] = readFileSync(`${markerPath(slug)}.attempts`, "utf8").trim().split(/\s+/);
    const count = Number(c);
    const lastExit = Number(e);
    if (!Number.isFinite(count) || count <= 0) return null;
    return { count, lastExit: Number.isFinite(lastExit) ? lastExit : -1 };
  } catch {
    return null;
  }
}

function scriptPath(slug: string): string {
  return join(DEV_DIR, `${slug}.sh`);
}

function readMarker(slug: string): "running" | "stopped" | "crashed" | null {
  try {
    const raw = readFileSync(markerPath(slug), "utf8").trim();
    return raw === "running" || raw === "stopped" || raw === "crashed" ? raw : null;
  } catch {
    return null;
  }
}

/** When the marker was last written, or null if it never was. */
function markerMtime(slug: string): number | null {
  try {
    // Rounded: mtimeMs is fractional, and this reaches `--json`.
    return Math.round(statSync(markerPath(slug)).mtimeMs);
  } catch {
    return null;
  }
}

export function devUrl(port: number): string {
  return requireDevServer().urlTemplate.replaceAll("{{port}}", String(port));
}

/**
 * Outcome of a loopback port probe. `unknown` is a real third answer,
 * not a synonym for `free`: see `probePort`.
 */
export type PortProbe = "listening" | "free" | "unknown";

/**
 * One loopback connect attempt. `ECONNREFUSED` is the only definitive
 * "nothing is listening" — on loopback a live server accepts and a dead
 * one refuses, both essentially instantly, so a *timeout* says nothing
 * about the port and everything about us.
 *
 * Specifically: the deadline is a timer on OUR event loop, and libuv
 * runs the timers phase before the poll phase. Block the loop past
 * `timeoutMs` (a heavy render, a big sync parse) and the timeout
 * callback fires ahead of a `connect` event that already landed — the
 * probe reports "free" for a port that is demonstrably listening.
 * Reproduced at a 500ms stall; wt has a 574ms loop block on record.
 * Hence `unknown`, never `free`.
 */
function probePortOnceEffect(port: number, timeoutMs: number): Effect.Effect<PortProbe> {
  return Effect.callback((resume) => {
    const sock = net.connect({ host: "127.0.0.1", port });
    let settled = false;
    const done = (result: PortProbe) => {
      if (settled) return;
      settled = true;
      sock.destroy();
      resume(Effect.succeed(result));
    };
    sock.setTimeout(timeoutMs, () => done("unknown"));
    sock.once("connect", () => done("listening"));
    sock.once("error", (err) =>
      done((err as NodeJS.ErrnoException).code === "ECONNREFUSED" ? "free" : "unknown"),
    );
    return Effect.sync(() => {
      sock.removeAllListeners();
      sock.destroy();
    });
  });
}

/**
 * Whether something accepts TCP connections on the port (loopback — the
 * server may bind wider, but loopback is always reachable when it is
 * up). An inconclusive first attempt is retried once: the usual cause
 * is our own loop having been blocked past the deadline, and by the
 * time we get here it is running again, so the second look almost
 * always resolves. Only twice-inconclusive is reported as `unknown`.
 */
export function probePort(port: number, timeoutMs = 400): Effect.Effect<PortProbe> {
  return Effect.flatMap(probePortOnceEffect(port, timeoutMs), (first) =>
    first === "unknown" ? probePortOnceEffect(port, timeoutMs) : Effect.succeed(first));
}

/**
 * Port-allocation view of the probe: anything but a definitive `free`
 * counts as taken. Handing out a port we merely failed to read would
 * collide with whatever is actually on it.
 */
function portInUseEffect(port: number): Effect.Effect<boolean> {
  return probePort(port).pipe(Effect.map((result) => result !== "free"));
}

/**
 * The slug's stable dev port. Reuses the persisted assignment when it's
 * still inside the configured range, unclaimed by another slug, and not
 * currently in use by a foreign process; otherwise scans the range for
 * the first free port and persists the new assignment. Callers must
 * have stopped this slug's own server first — a port our own session
 * holds would otherwise read as foreign and force a pointless
 * reallocation.
 */
export const allocateDevPort = Effect.fn("allocateDevPort")(function* (slug: string) {
  const dev = requireDevServer();
  const state = readWtState();
  const claimed = new Set<number>();
  for (const [s, rec] of Object.entries(state.slugs)) {
    if (s !== slug && rec.devPort !== undefined) claimed.add(rec.devPort);
  }
  const inRange = (p: number) => p >= dev.portBase && p < dev.portBase + dev.portRange;
  const recorded = state.slugs[slug]?.devPort;
  if (
    recorded !== undefined &&
    inRange(recorded) &&
    !claimed.has(recorded) &&
    !(yield* portInUseEffect(recorded))
  ) {
    return recorded;
  }
  // Probe (async, unlocked) for a handful of OS-level-free candidates,
  // then claim atomically: `claimDevPort` re-checks the wtstate record
  // under the repository-state lock, so two processes allocating concurrently
  // can't both persist the same port — the loser lands on the next
  // candidate. Several candidates so one lost race doesn't force a
  // rescan.
  const candidates: number[] = [];
  for (let p = dev.portBase; p < dev.portBase + dev.portRange; p++) {
    if (claimed.has(p)) continue;
    if (yield* portInUseEffect(p)) continue;
    candidates.push(p);
    if (candidates.length >= 8) break;
  }
  const port = claimDevPort(slug, candidates);
  if (port === null) {
    return yield* new DevServerOperationError({
      slug,
      operation: "port",
      cause:
        `no free dev-server port in ${dev.portBase}-${dev.portBase + dev.portRange - 1}; ` +
        "stop something or widen [dev_server] port_range",
    });
  }
  if (recorded !== undefined && recorded !== port) {
    log.info("reallocated dev port", { slug, from: recorded, to: port });
  }
  return port;
});


// ---------------------------------------------------------------------------
// Concurrency slots
// ---------------------------------------------------------------------------

/**
 * The fleet-wide waiting room: one `<slug>.json` per queued starter,
 * holding the waiting process's pid and when it joined.
 *
 * A file, and not a counter, because the accounting has to survive
 * crashes. wt's rule for encoded state is that it be write-once and
 * self-expiring, and this is: written once when a `--wait` begins, and
 * validated on every read against whether that pid is still alive. A
 * waiter killed with SIGKILL leaves a file that the next reader ignores
 * and deletes. There is no release call to forget.
 */
const WAIT_DIR = join(DEV_DIR, "waiting");

/** How long a `--wait` polls before giving up, when no timeout is given. */
export const DEV_WAIT_DEFAULT_TIMEOUT_MS = 30 * 60_000;
/** Gap between slot polls while waiting. */
const DEV_WAIT_POLL_MS = 3_000;
/**
 * How often a waiting starter re-runs the reclaim sweep. The sweep
 * lists worktrees (git) and may shell out to `stop_command`, so it is
 * far too heavy for every poll; the leak it clears took hours to
 * appear, so a minute of latency on clearing it costs nothing.
 */
const DEV_WAIT_RECLAIM_EVERY_MS = 60_000;

export type DevWaiter = {
  slug: string;
  pid: number;
  since: number;
  /**
   * Queue tier. 0 is everyone; `DEV_QUEUE_FIRST` jumps ahead of every
   * 0, with `since` still ordering within a tier.
   *
   * A tier and not an index, because an index is a total order and a
   * total order has to be renumbered on every insert, every departure
   * and every pruned dead waiter — while the thing anyone actually
   * wants to say is "this one goes first", which is a property of the
   * waiter rather than a position in a list. It also inherits the
   * waiting room's self-expiry for free: the priority lives in the
   * waiter's own file and is gone the moment that pid is, so nothing
   * has to remember to reset it.
   */
  priority: number;
};

/** The one non-default tier: ahead of every ordinary waiter. */
export const DEV_QUEUE_FIRST = 1;

/**
 * One dev server occupying a slot. `crashed` is a holder like any
 * other: the supervisor parked, but the resources the command created
 * outside its own process tree — the containers, the tunnels — are
 * still up, and those are what the cap is actually rationing. Reporting
 * it separately is so the refusal message can point at the cheapest
 * slot to reclaim.
 */
export type DevSlotHolder = { slug: string; state: "up" | "crashed" };

export type DevSlotReport = {
  /** `[dev_server] max_concurrent`, or null when uncapped. */
  limit: number | null;
  /** Current holders, or null when tmux could not be queried. */
  holders: DevSlotHolder[] | null;
  /** Live waiters, oldest first — the queue order. */
  waiters: DevWaiter[];
  /** Slots left, or null when uncapped or tmux state is unavailable. */
  free: number | null;
};

function waiterPath(slug: string): string {
  return join(WAIT_DIR, `${slug}.json`);
}

/**
 * Whether a pid is still around. `EPERM` counts as alive — the process
 * exists, it just isn't ours to signal. Only `ESRCH` is dead.
 */
function pidAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    return (err as NodeJS.ErrnoException).code === "EPERM";
  }
}

/**
 * Live waiters, oldest first, pruning the files of processes that are
 * gone. The prune is the whole reason this can be trusted: nothing ever
 * has to remember to leave the queue.
 */
export function readDevWaiters(dir: string = WAIT_DIR): DevWaiter[] {
  let names: string[];
  try {
    names = readdirSync(dir);
  } catch {
    return []; // Dir doesn't exist until the first `--wait`.
  }
  const waiters: DevWaiter[] = [];
  for (const name of names) {
    if (!name.endsWith(".json")) continue;
    const slug = name.slice(0, -".json".length);
    let rec: { pid?: unknown; since?: unknown; priority?: unknown };
    try {
      rec = JSON.parse(readFileSync(join(dir, name), "utf8"));
    } catch {
      // A torn or hand-edited file is not a waiter. Drop it.
      try {
        rmSync(join(dir, name), { force: true });
      } catch {
        // Advisory.
      }
      continue;
    }
    const pid = typeof rec.pid === "number" ? rec.pid : null;
    const since = typeof rec.since === "number" ? rec.since : null;
    const priority =
      typeof rec.priority === "number" && Number.isFinite(rec.priority) ? rec.priority : 0;
    if (pid === null || since === null || !pidAlive(pid)) {
      try {
        rmSync(join(dir, name), { force: true });
      } catch {
        // Advisory.
      }
      continue;
    }
    waiters.push({ slug, pid, since, priority });
  }
  // Tier first, then arrival. Within a tier this is exactly the FIFO it
  // has always been.
  return waiters.sort(
    (a, b) => b.priority - a.priority || a.since - b.since || a.slug.localeCompare(b.slug),
  );
}

/**
 * Move an already-queued waiter between tiers. Returns the updated
 * waiter, or null when the slug isn't in the queue.
 *
 * Deliberately edits an EXISTING waiter rather than recording a
 * priority for a slug that might queue later: a priority with no waiter
 * attached has nothing to expire it, and would sit in the cache
 * steering a decision made weeks ago. It also means promotion needs no
 * cooperation from the promoted agent — its own poll re-reads the queue
 * and finds itself at the front. That is the whole point: a message
 * asking an agent to act loses the race against a slot that frees
 * instantly, and this doesn't race at all.
 */
export function setDevWaiterPriority(slug: string, priority: number): DevWaiter | null {
  const current = readDevWaiters().find((w) => w.slug === slug);
  if (!current) return null;
  const next: DevWaiter = { ...current, priority };
  try {
    writeFileSync(
      waiterPath(slug),
      JSON.stringify({ pid: next.pid, since: next.since, priority }),
    );
  } catch (err) {
    log.warn("could not set dev-queue priority", {
      slug,
      err: err instanceof Error ? err.message : String(err),
    });
    return null;
  }
  log.attention.info(
    priority > 0
      ? `${slug} moved to the front of the dev-server queue`
      : `${slug} returned to its place in the dev-server queue`,
  );
  return next;
}

function joinDevQueue(slug: string): void {
  try {
    mkdirSync(WAIT_DIR, { recursive: true });
    writeFileSync(
      waiterPath(slug),
      JSON.stringify({ pid: process.pid, since: Date.now() }),
    );
  } catch (err) {
    // The queue is a visibility and fairness aid, not a lock — a
    // failure here degrades to "starts race for free slots", which is
    // the behavior without the queue at all.
    log.warn("could not join the dev-server queue", {
      slug,
      err: err instanceof Error ? err.message : String(err),
    });
  }
}

function leaveDevQueue(slug: string): void {
  try {
    rmSync(waiterPath(slug), { force: true });
  } catch {
    // Left behind at worst; the pid check prunes it on the next read.
  }
}

/** Slugs with a live `<slug>-dev` tmux session, or null if tmux couldn't be asked. */
const devSessionSlugsEffect = Effect.fnUntraced(function* (): Effect.fn.Return<string[] | null> {
  const names = yield* probeSessionNames();
  if (names === null) return null;
  const slugs: string[] = [];
  for (const name of names) {
    if (name.endsWith(SUFFIX.dev)) slugs.push(name.slice(0, -SUFFIX.dev.length));
  }
  return slugs;
});
/**
 * Who currently holds a slot. Derived from tmux every time — there is
 * no ledger to drift, and a slot frees itself when its session goes,
 * whether that was a stop, a crash, a destroy, or a killed tmux server.
 *
 * `null` (tmux unreachable) is propagated rather than collapsed to an
 * empty list: an unanswerable question must not read as "nothing is
 * running", which would let the cap wave everything through at exactly
 * the moment wt has lost track of the fleet.
 */
export const devSlotHolders = Effect.fn("devSlotHolders")(function* (): Effect.fn.Return<DevSlotHolder[] | null> {
  const slugs = yield* devSessionSlugsEffect();
  if (slugs === null) return null;
  return slugs
    .map((slug): DevSlotHolder => ({
      slug,
      state: readMarker(slug) === "crashed" ? "crashed" : "up",
    }))
    .sort((a, b) => a.slug.localeCompare(b.slug));
});

/** Slots, holders and queue in one shot — the `wt dev status --all` view. */
export const devSlotReport = Effect.fn("devSlotReport")(function* (): Effect.fn.Return<DevSlotReport> {
  const limit = config.devServer?.maxConcurrent ?? null;
  const holders = yield* devSlotHolders();
  return {
    limit,
    holders,
    waiters: readDevWaiters(),
    free: limit === null || holders === null ? null : Math.max(0, limit - holders.length),
  };
});

/**
 * Run the project's `stop_command` for a slug whose dev server has just
 * gone down (or is being reclaimed). Never fatal — see
 * `runTeardownCommand`.
 *
 * `cwd` falls back to the main clone when the checkout is gone, which
 * is the reclaim case: an orphaned dev session outlived its worktree.
 * The teardown still has the slug and the port, which is what a
 * container-name or port-block filter needs.
 */
/**
 * Returns whether the teardown was confirmed: the hook succeeded,
 * or there is no hook to run. A failed hook leaves external resources
 * in an unknown state. The distinction only matters to
 * `resetDevServer` — a plain stop reports the failure and moves on,
 * exactly as a destroy does.
 */
function runDevStopCommandEffect(
  slug: string,
  path: string | null,
  onLog?: (line: string) => void,
): Effect.Effect<boolean> {
  const template = config.devServer?.stopCommand ?? null;
  const command = resolveTeardownCommand(template, {
    path: path ?? config.paths.mainClone,
    slug,
    port: readWtState().slugs[slug]?.devPort ?? null,
  });
  if (!command) return Effect.succeed(true);
  return runTeardownCommand({
    label: "stop_command",
    command,
    cwd: path ?? config.paths.mainClone,
    slug,
    onLog: onLog ?? ((line) => log.info(line, { slug })),
  });
}
/**
 * Reclaim slots held by dev sessions that belong to no live worktree.
 * Run when an acquire finds the fleet full, never on the happy path:
 * the sweep is the answer to "the counter drifted", and there is
 * nothing to answer while slots are free.
 *
 * Scope is deliberately narrow — a session whose worktree is gone owns
 * nothing anyone can still want. A dev server for a worktree that still
 * exists is somebody's, even if no agent is attached to it right now,
 * and taking it would be wt reaching into another slug's work.
 *
 * Returns the slugs reclaimed.
 */
export const reclaimDevSlots = Effect.fn("reclaimDevSlots")(function* (): Effect.fn.Return<string[]> {
  const slugs = yield* devSessionSlugsEffect();
  if (slugs === null || slugs.length === 0) return [];
  const worktrees = yield* Effect.result(listWorktrees());
  if (worktrees._tag === "Failure") {
    // Can't establish what's live — reclaiming on a guess would kill a
    // working dev server. Leave the fleet as it is.
    return [];
  }
  const live = new Set(worktrees.success.map((w) => w.slug));
  const orphans = slugs.filter((slug) => !live.has(slug));
  if (orphans.length === 0) return [];
  for (const slug of orphans) {
    log.attention.warn(`reclaiming dev-server slot from orphaned ${slug}`);
    yield* captureDevCrashLogEffect(slug);
    yield* killByName(sessionName(slug, "dev"));
    yield* runDevStopCommandEffect(slug, null);
  }
  return orphans;
});

/**
 * Thrown by `resetDevServer` when `stop_command` failed, so the
 * environment's state is unknown and must not be discarded. Carries
 * the slug for the message; the
 * remedy is always the project's own teardown, which wt cannot name.
 */
export class DevResetStopFailedError extends Error {
  constructor(readonly slug: string) {
    super(`stop_command failed for ${slug} — external resources are unconfirmed; reset_command was not run`);
    this.name = "DevResetStopFailedError";
  }
}

/**
 * Thrown by `startDevServer` when `[dev_server] max_concurrent` is
 * reached. Carries the holders so the caller can say WHO has the slots
 * — a refusal that doesn't name what to free just moves the question to
 * the human, which is the opposite of the point.
 */
export class DevSlotFullError extends Error {
  readonly holders: DevSlotHolder[];
  readonly limit: number;
  /**
   * Non-empty when a slot was actually free and is being held for a
   * promoted waiter. The two refusals need different next actions —
   * "the fleet is at capacity" says free something, "you are behind X"
   * says queue up — so the caller must be able to tell them apart
   * rather than reading a capacity number that isn't the reason.
   */
  readonly yieldingTo: DevWaiter[];
  constructor(decision: DevSlotDecision) {
    const limit = decision.limit ?? 0;
    const yieldingTo = decision.yieldingTo ?? [];
    super(
      yieldingTo.length > 0
        ? `a dev-server slot is free but held for ${yieldingTo.map((w) => w.slug).join(", ")}`
        : `dev-server slots full (${decision.holders.length}/${limit}): ` +
            decision.holders.map((h) => h.slug).join(", "),
    );
    this.name = "DevSlotFullError";
    this.holders = decision.holders;
    this.limit = limit;
    this.yieldingTo = yieldingTo;
  }
}

export type DevSlotDecision = {
  /** True when a start may proceed now. */
  ok: boolean;
  limit: number | null;
  /** Slots left, or null when uncapped. */
  free: number | null;
  /** Everyone holding a slot right now, excluding the asking slug. */
  holders: DevSlotHolder[];
  /**
   * Set when a slot was free but is being held for a promoted waiter.
   * A distinct reason from "full", and the caller says so: "wait your
   * turn behind X" and "the fleet is at capacity" want different next
   * actions.
   */
  yieldingTo?: DevWaiter[];
};

/**
 * Whether `slug` may start a dev server. The asking slug never counts
 * against itself: `wt dev start` is also restart, and relaunching a
 * server that is already running adds no load — refusing it would make
 * a full fleet unable to pick up a config edit.
 *
 * When the fleet is full and `reclaim` is set, orphaned sessions are
 * swept and the count retaken before refusing.
 *
 * This is a load governor, not a mutex, and the gap between deciding
 * and the new session existing is real: two starts racing at exactly
 * the same instant can both see the last slot. That overshoot is one
 * dev server for as long as one of them runs, which is the failure this
 * feature can afford; a genuine lock across independent CLI processes
 * would have to be released, and a lock that must be released is the
 * drifting counter this design exists to avoid.
 */
export const checkDevSlot = Effect.fn("checkDevSlot")(function* (
  slug: string,
  opts: { reclaim?: boolean; respectPriority?: boolean } = {},
): Effect.fn.Return<DevSlotDecision, DevServerOperationError> {
  const limit = config.devServer?.maxConcurrent ?? null;
  if (limit === null) return decideDevSlot(slug, [], null);
  let holders = yield* devSlotHolders();
  if (holders === null) {
    return yield* new DevServerOperationError({
      slug,
      operation: "slot",
      cause: "tmux session inventory is unavailable",
    });
  }
  if (!decideDevSlot(slug, holders, limit).ok && opts.reclaim !== false) {
    if ((yield* reclaimDevSlots()).length > 0) {
      const refreshed = yield* devSlotHolders();
      if (refreshed === null) {
        return yield* new DevServerOperationError({
          slug,
          operation: "slot",
          cause: "tmux session inventory became unavailable after reclaim",
        });
      }
      holders = refreshed;
    }
  }
  const decision = decideDevSlot(slug, holders, limit);
  if (!decision.ok || opts.respectPriority === false) return decision;
  // A plain `wt dev start` normally takes any free slot without
  // consulting the queue — being told "full" while `wt dev status`
  // shows a free slot is a worse lie than the occasional queue-jump,
  // and jumping only ever costs an ordinary waiter one poll interval.
  //
  // A PROMOTED waiter is different in kind. Someone with fleet context
  // said this one goes first, and a barge past it silently defeats a
  // deliberate decision rather than a default. So a free slot is
  // withheld from anyone the promoted waiter is ahead of — which is
  // everyone except itself.
  const promoted = readDevWaiters().filter((w) => w.priority > 0 && w.slug !== slug);
  if (promoted.length >= decision.free!) {
    return { ...decision, ok: false, yieldingTo: promoted.slice(0, decision.free!) };
  }
  return decision;
});

/**
 * The cap arithmetic, split from the tmux read so the rule is testable.
 *
 * `slug` is filtered out of its own holder list: `wt dev start` is also
 * restart, and relaunching a server that is already up adds no load.
 * Without that, a full fleet could never pick up a config edit — every
 * restart would be refused by the server it was restarting.
 */
export function decideDevSlot(
  slug: string,
  holders: readonly DevSlotHolder[],
  limit: number | null,
): DevSlotDecision {
  const others = holders.filter((h) => h.slug !== slug);
  if (limit === null) return { ok: true, limit: null, free: null, holders: others };
  const free = Math.max(0, limit - others.length);
  return { ok: free > 0, limit, free, holders: others };
}

/**
 * Queue for a slot, resolving once one is available (or `false` on
 * timeout). Joins the visible waiting room for the duration, so a
 * blocked start shows on the board and in `wt dev status --all`
 * instead of looking like an agent that stopped working.
 *
 * Fairness is FIFO among waiters only: a waiter takes a slot when its
 * rank in the queue is inside the number of free slots. A plain
 * (non-waiting) `wt dev start` is never held back by the queue — being
 * told "full" while `wt dev status` shows a free slot would be a worse
 * lie than the occasional queue-jump, and the jumper is only ever
 * taking a slot that was genuinely free.
 */
type DevSlotWaitDependencies = {
  readonly check: (
    slug: string,
    opts: { reclaim: boolean },
  ) => Effect.Effect<DevSlotDecision, DevServerOperationError>;
  readonly waiters: typeof readDevWaiters;
  readonly join: typeof joinDevQueue;
  readonly leave: typeof leaveDevQueue;
};

const devSlotWaitDependencies: DevSlotWaitDependencies = {
  check: (slug, opts) => checkDevSlot(slug, opts).pipe(
    Effect.mapError((cause) => new DevServerOperationError({ slug, operation: "slot", cause })),
  ),
  waiters: readDevWaiters,
  join: joinDevQueue,
  leave: leaveDevQueue,
};

export type WaitForDevSlotOptions = {
  timeoutMs?: number;
  /**
   * Absolute deadline (epoch ms), for a caller sharing one budget across
   * this wait and a following `waitForDevReady` — `wt dev start --wait
   * --timeout` waits for a slot and then for readiness, and without a
   * shared deadline each phase got the FULL timeout, doubling the
   * effective budget. Takes precedence over `timeoutMs` when set.
   */
  deadlineMs?: number;
  /** Called on each poll that doesn't get a slot. */
  onWait?: (info: { rank: number; holders: DevSlotHolder[]; waited: number }) => void;
};

export const waitForDevSlot = Effect.fn("waitForDevSlot")(function* (
  slug: string,
  opts: WaitForDevSlotOptions = {},
  dependencies: DevSlotWaitDependencies = devSlotWaitDependencies,
): Effect.fn.Return<boolean, DevServerOperationError> {
  const started = yield* Clock.currentTimeMillis;
  const deadline = opts.deadlineMs ?? started + (opts.timeoutMs ?? DEV_WAIT_DEFAULT_TIMEOUT_MS);
  let lastReclaim = started - DEV_WAIT_RECLAIM_EVERY_MS;
  const poll = (): Effect.Effect<boolean, DevServerOperationError> =>
    Effect.gen(function* () {
      const now = yield* Clock.currentTimeMillis;
      const reclaim = now - lastReclaim >= DEV_WAIT_RECLAIM_EVERY_MS;
      if (reclaim) lastReclaim = now;
      const decision = yield* dependencies.check(slug, { reclaim });
      const checkedAt = yield* Clock.currentTimeMillis;
      const waiters = dependencies.waiters();
      const rank = waiters.findIndex((w) => w.slug === slug);
      // rank < 0 means our own file went missing (a hand-cleaned cache,
      // a full disk). Fall back to first-come rather than waiting for a
      // position we no longer hold.
      if (decision.ok && (decision.free === null || rank < 0 || rank < decision.free)) {
        return true;
      }
      if (checkedAt >= deadline) return false;
      opts.onWait?.({
        rank: rank < 0 ? 0 : rank,
        holders: decision.holders,
        waited: checkedAt - started,
      });
      yield* Effect.sleep(Duration.millis(DEV_WAIT_POLL_MS));
      return yield* Effect.suspend(poll);
    });
  return yield* Effect.acquireUseRelease(
    Effect.sync(() => dependencies.join(slug)),
    poll,
    () => Effect.sync(() => dependencies.leave(slug)),
  );
});

/**
 * Save a parked supervisor's scrollback next to its marker before
 * anything kills the pane. `remain-on-exit` keeps the crash report
 * readable, but only for as long as the session lives, and reclaim
 * kills sessions — losing the one artifact that says why the server
 * died would make the sweep worse than the leak. `wt dev logs` falls
 * back to this file when the pane is gone.
 */
const captureDevCrashLogEffect = Effect.fnUntraced(function* (slug: string): Effect.fn.Return<void> {
  if (readMarker(slug) !== "crashed") return;
  const text = yield* capturePane(sessionName(slug, "dev"));
  if (!text) return;
  // Best-effort; the pane is about to go either way.
  yield* Effect.try({
    try: () => {
      mkdirSync(DEV_DIR, { recursive: true });
      writeFileSync(crashLogPath(slug), text);
    },
    catch: (cause) => new DevServerOperationError({ slug, operation: "status", cause }),
  }).pipe(Effect.ignore);
});

function crashLogPath(slug: string): string {
  return join(DEV_DIR, `${slug}.crash.log`);
}

/** The saved crash scrollback for a slug, or null if there isn't one. */
export function readDevCrashLog(slug: string): string | null {
  try {
    return readFileSync(crashLogPath(slug), "utf8");
  } catch {
    return null;
  }
}

/**
 * The supervisor script. POSIX sh (runs under /bin/sh); the user's
 * command is spliced in verbatim — it's trusted config, the same
 * standing as `[[actions]]` shell strings. `$PORT` is exported for
 * commands that read the environment instead of `{{port}}`.
 */
/**
 * Path to the `wt` launcher, for the generated supervisor to call back
 * into. Same derivation the detached `_destroy` spawn uses.
 */
function wtExecPath(): string {
  return join(import.meta.dir, "..", "..", "bin", "wt");
}

/**
 * What the supervisor hands back when it parks: save the scrollback,
 * tear the stack down, and release the slot.
 *
 * The ordering is the whole point. The pane is captured FIRST, because
 * it holds the only copy of why the server died and the next two steps
 * destroy it — a project guard's refusal (the port-base disagreement
 * that names both bases and the fix) is exactly the artifact that ends
 * an investigation in seconds, and it was being thrown away in favour
 * of "dev server crashed while starting", which says the opposite of
 * what happened.
 *
 * Then `stop_command`, because a parked supervisor is the one case
 * where the process wt supervises is gone while everything it created
 * outside its own process tree is still up. Twelve containers can
 * outlive it indefinitely; measured survivors were nineteen hours old.
 *
 * Then the session, which is what frees the slot. There is no new
 * accounting for that: a slot is derived from live dev sessions, so
 * ending the session releases it the same way a stop or a destroy
 * does. The marker deliberately stays `crashed`, and `devServerStatus`
 * resolves "no session + crashed marker" to a crashed row — so the
 * board still shows what happened, `wt dev logs` reads the saved
 * scrollback, and the fleet stops queueing behind a slot that is
 * holding nothing.
 */
export const handleDevGiveUp = Effect.fn("handleDevGiveUp")(function* (slug: string) {
  yield* captureDevCrashLogEffect(slug);
  const wt = (yield* listWorktrees()).find((w) => w.slug === slug) ?? null;
  yield* runDevStopCommandEffect(slug, wt?.path ?? null);
  yield* run([
    "tmux", "-L", TMUX_SOCKET, "kill-session", "-t", `=${sessionName(slug, "dev")}`,
  ]);
});

export function supervisorScript(slug: string, command: string, port: number): string {
  const session = sessionName(slug, "dev");
  return `#!/bin/sh
# Generated by wt — [dev_server] supervisor for ${slug}. Regenerated on
# every start; do not edit.
STATE=${shQuote(markerPath(slug))}
PORT=${port}
export PORT
# Survive the signals that kill the (un-trapped) child: Ctrl-C in an
# attached pane delivers SIGINT to the whole foreground group, and
# without this trap the supervisor itself dies before it can observe
# the child's exit code — the intentional-stop branch below would be
# unreachable and the marker would stay "running" forever.
trap : INT TERM
fails=0
prev_sig=''
delay=${RESTART_DELAY_SECONDS}
while :; do
  printf running > "$STATE"
  started=$(date +%s)
  ${command}
  code=$?
  # SIGINT/SIGTERM = someone stopped it on purpose (Ctrl-C in an
  # attached pane, a stray kill). Don't fight them — end the session.
  if [ "$code" -eq 130 ] || [ "$code" -eq 143 ]; then
    printf stopped > "$STATE"
    echo "wt: dev server stopped (exit $code)"
    exec tmux -L ${TMUX_SOCKET} kill-session -t ${shQuote(`=${session}`)}
  fi
  ran=$(($(date +%s) - started))
  # Failure signature: exit code + the child's last non-empty output
  # line. Captured HERE, before the supervisor echoes anything of its
  # own, or the next pass would compare wt's restart notice with
  # itself and every failure would look deterministic.
  last=$(tmux -L ${TMUX_SOCKET} capture-pane -p -t ${shQuote(paneTarget(session))} 2>/dev/null | grep -v '^[[:space:]]*$' | tail -1)
  # No captured line means the signature is UNKNOWN, not "empty". An
  # empty string would compare equal to the next unknown one, silently
  # collapsing the two-signal test to exit-code-only — and everything
  # fails with 1, so that parks genuine flakes on their second try.
  # Unknown must fail toward taking the retries.
  if [ -n "$last" ]; then sig="$code|$last"; else sig=''; fi
  if [ "$ran" -lt ${ESTABLISHED_AFTER_SECONDS} ]; then
    fails=$((fails + 1))
  else
    fails=0
    delay=${RESTART_DELAY_SECONDS}
    prev_sig=''
  fi
  # Attempt count and last exit, so a restart loop is visible as one
  # rather than looking like a slow start. Read by devServerStatus.
  printf '%s %s' "$fails" "$code" > "$STATE.attempts"
  # A fast failure that repeats itself exactly is a guard, not a crash:
  # it has already printed the cause and the fix, and the remaining
  # attempts only delay the human reading them.
  if [ "$ran" -lt ${DETERMINISTIC_FAILURE_SECONDS} ] && [ -n "$prev_sig" ] && [ "$sig" = "$prev_sig" ]; then
    fails=${GIVE_UP_AFTER}
    echo "wt: same failure twice in a row (exit $code, under ${DETERMINISTIC_FAILURE_SECONDS}s) — not retrying."
  fi
  prev_sig="$sig"
  if [ "$fails" -ge ${GIVE_UP_AFTER} ]; then
    printf crashed > "$STATE"
    echo "wt: dev server failed to establish (last exit $code) — giving up."
    # Single quotes: backticks in a double-quoted sh string would run as
    # command substitution.
    echo 'wt: fix the cause, then start it again from the ! menu or "wt dev start".'
    # Hand back to wt: save this pane's scrollback (the only copy of why
    # it died), run the project's stop_command so the stack does not
    # outlive the supervisor, and end the session so the slot frees.
    # The marker stays "crashed", so the row still reads crashed with no
    # session — devServerStatus resolves that case explicitly.
    ${shQuote(wtExecPath())} _dev-giveup ${shQuote(slug)} || true
    exit 1
  fi
  echo "wt: dev server exited ($code) after \${ran}s — restarting in \${delay}s (attempt $((fails + 1)) of ${GIVE_UP_AFTER})"
  sleep "$delay"
  delay=$((delay * 2))
  [ "$delay" -gt ${MAX_RESTART_DELAY_SECONDS} ] && delay=${MAX_RESTART_DELAY_SECONDS}
done
`;
}

/**
 * Start — or restart — the slug's dev server. Idempotent by design:
 * any existing session is killed first, the port re-resolved, the
 * supervisor script rewritten (so config edits take effect), and a
 * fresh session spawned in the worktree. `remain-on-exit` keeps a
 * crashed supervisor's pane (and therefore the crash logs) readable;
 * intentional stops self-kill the session instead.
 */
export const startDevServer = Effect.fn("startDevServer")(function* (wt: {
  slug: string;
  path: string;
}) {
  const dev = requireDevServer();
  // ADOPT a start already in flight rather than killing it.
  //
  // `start` is also `restart`, and killing a RUNNING server to bring it
  // back on new config is the point of that. A supervisor still in its
  // STARTUP phase is a different thing, and killing one is destructive
  // in a way nothing downstream can see. `wt dev reset` ends by
  // launching, returns 0, and the banner then recommended `wt dev start
  // --wait` to wait for it — so wt walked the reader into killing the
  // launch it had just made, mid-`supabase start`, against a database
  // whose volumes the reset had just dropped.
  //
  // The two reported faces are what a kill at two different moments
  // looks like: `SqlError: Connection error` while the baseline
  // migration applies, and "did not return the local URL and
  // service-role key" with every service stopped. The residue is worse
  // than the crash. A half-initialised database VOLUME survives, so the
  // next start finds an existing database, takes its reuse path, and
  // never re-provisions what only a fresh initialisation creates — a
  // storage catalog reading 0 of 22 buckets while the migration ledger
  // reports fully migrated, because the migrations really did apply.
  // That also explains why it is intermittent: a kill after the
  // database is up leaves a complete volume and costs nothing, and only
  // a kill DURING initialisation leaves the broken one.
  //
  // Deliberately keyed on `starting` rather than "a session exists":
  // restarting a running server and recycling a parked/crashed one both
  // stay exactly as they were.
  const inFlight = yield* devServerStatus(wt.slug, { path: wt.path });
  if (inFlight.starting && inFlight.port !== null) {
    log.event.info(
      `dev server already starting on port ${inFlight.port} — joined it (${wt.slug})`,
    );
    return { port: inFlight.port, url: devUrl(inFlight.port), adopted: true };
  }
  // The cap is enforced here rather than in the CLI so a future caller
  // inherits it — the whole point of a load governor is that there is
  // no path around it. `wt dev start --wait` queues first and lands
  // here with a slot already free.
  const slot = yield* checkDevSlot(wt.slug, { reclaim: true });
  if (!slot.ok) throw new DevSlotFullError(slot);
  const session = sessionName(wt.slug, "dev");
  // Kill directly rather than through `stopDevServer`: start is also
  // restart, and restarting must NOT take the user's browser tabs with
  // it — they're about to be pointed at the same port again.
  yield* killByName(session);
  // A just-killed server takes a beat to release its socket; wait for
  // the recorded port to actually close so the allocator doesn't
  // mistake our own dying process for a foreign one and churn the slug
  // to a new port on every restart. Bounded — a port that stays busy
  // past the wait really is foreign, and reallocation is then correct.
  const recorded = readWtState().slugs[wt.slug]?.devPort;
  if (recorded !== undefined) {
    const deadline = (yield* Clock.currentTimeMillis) + 3_000;
    while ((yield* Clock.currentTimeMillis) < deadline && (yield* portInUseEffect(recorded))) {
      yield* Effect.sleep(Duration.millis(150));
    }
  }
  const port = yield* allocateDevPort(wt.slug);
  const command = dev.command.replaceAll("{{port}}", String(port));
  const script = scriptPath(wt.slug);
  yield* Effect.try({
    try: () => {
      mkdirSync(DEV_DIR, { recursive: true });
      writeFileSync(script, supervisorScript(wt.slug, command, port), { mode: 0o755 });
    },
    catch: (cause) => new DevServerOperationError({ slug: wt.slug, operation: "start", cause }),
  });

  const userShell = process.env.SHELL || "/bin/bash";
  // Outer login shell loads the user's PATH/env (npm, nvm, direnv…);
  // /bin/sh then runs the generated POSIX script with that env.
  const spawn = yield* run([
    "tmux",
    "-L",
    TMUX_SOCKET,
    "new-session",
    "-d",
    "-s",
    session,
    "-c",
    wt.path,
    userShell,
    "-lc",
    `exec /bin/sh ${shQuote(script)}`,
  ]);
  if (spawn.exitCode !== 0) {
    throw new Error(
      `could not start dev server session: ${(spawn.stderr || spawn.stdout || `tmux exit ${spawn.exitCode}`).trim()}`,
    );
  }
  // Keep the pane after the supervisor parks on a crash — the scrollback
  // IS the crash report. `remain-on-exit` is a WINDOW option, so it needs
  // `-w` and a window-resolving target (the trailing `:` = exact session,
  // active window; the bare `=name` form only resolves sessions).
  // Best-effort; a failure just loses that nicety.
  const remain = yield* run([
    "tmux",
    "-L",
    TMUX_SOCKET,
    "set-option",
    "-w",
    "-t",
    `=${session}:`,
    "remain-on-exit",
    "on",
  ]);
  if (remain.exitCode !== 0) {
    log.warn("could not set remain-on-exit on dev session", {
      slug: wt.slug,
      stderr: remain.stderr.slice(0, 200),
    });
  }
  // Anchor the run to the commit it came up on. Written after the
  // session is confirmed spawned, so a failed start doesn't move the
  // anchor and make a stale environment look fresh.
  setSlugDevStartedSha(wt.slug, yield* revParse("HEAD", wt.path));
  log.event.info(`dev server starting on port ${port} (${wt.slug})`);
  return { port, url: devUrl(port) };
});

/**
 * Remove the slug's on-disk supervisor artifacts (marker + script).
 * Called from `createWorktree`'s stale-state reset so a re-created slug
 * can't inherit a dead predecessor's `crashed` marker — the same
 * fresh-start guarantee `clearSlugState` gives the wtstate record.
 */
export function clearDevServerFiles(slug: string): void {
  for (const p of [
    markerPath(slug),
    `${markerPath(slug)}.attempts`,
    scriptPath(slug),
    crashLogPath(slug),
    waiterPath(slug),
  ]) {
    try {
      rmSync(p, { force: true });
    } catch {
      // Advisory files; a failed cleanup surfaces as a stale marker at worst.
    }
  }
}

/**
 * Startup sweep: drop marker/script files for slugs that no longer
 * exist, mirroring `reapWtState`/`reapShellLogs` — a destroyed slug's
 * files are otherwise orphaned forever (`clearDevServerFiles` only
 * covers same-slug re-creates). Best-effort.
 */
export function reapDevServerFiles(liveSlugs: ReadonlySet<string>): void {
  let entries: string[];
  try {
    entries = readdirSync(DEV_DIR);
  } catch {
    return; // Dir doesn't exist until the first start — nothing to sweep.
  }
  for (const name of entries) {
    const slug = name.replace(/\.(state|state\.attempts|sh|crash\.log)$/, "");
    if (slug === name || liveSlugs.has(slug)) continue;
    try {
      rmSync(join(DEV_DIR, name), { force: true });
    } catch {
      // Best-effort; a leftover file is cosmetic.
    }
  }
  // The waiting room needs no liveness rule of its own — every entry is
  // validated against its pid on read — but a dead slug's file would
  // otherwise sit there until someone happened to read the queue.
  for (const waiter of readDevWaiters()) {
    if (liveSlugs.has(waiter.slug)) continue;
    try {
      rmSync(waiterPath(waiter.slug), { force: true });
    } catch {
      // Best-effort.
    }
  }
}

/**
 * Stop the slug's dev server. Idempotent; the port stays reserved.
 *
 * Killing the session is only half of a stop. A dev command routinely
 * hands work to something that is not its child — `supabase start`
 * returns once the docker daemon has the containers — so the process
 * tree going away releases nothing. `[dev_server] stop_command` is the
 * project's chance to say what else to take down; without it, "stopped"
 * means the vite process is gone and the twelve containers are not.
 */
export const stopDevServer = Effect.fn("stopDevServer")(function* (
  wt: { slug: string; path: string },
  onLog?: (line: string) => void,
) {
  const slug = wt.slug;
  yield* killByName(sessionName(slug, "dev"));
  // Marker is advisory; the killed session already means "not running".
  yield* Effect.try({
    try: () => {
      mkdirSync(DEV_DIR, { recursive: true });
      writeFileSync(markerPath(slug), "stopped");
    },
    catch: (cause) => new DevServerOperationError({ slug, operation: "status", cause }),
  }).pipe(Effect.ignore);
  // After the kill, so the teardown isn't racing a supervisor that is
  // about to restart the command it just tore down.
  const stopped = yield* runDevStopCommandEffect(slug, wt.path, onLog);
  if (stopped) log.event.info(`dev server stopped (${slug})`);
  else log.attention.warn(`dev supervisor stopped, but stop_command failed; external resources unconfirmed (${slug})`);
  // The tabs pointed at this server are stranded on a refused port the
  // moment it goes down, so they go with it — same reflex as destroy,
  // narrowed to the port (an agent's other tabs aren't the server's).
  // After the kill, never before: a stop that failed to take leaves a
  // server the user is still browsing.
  const port = readWtState().slugs[slug]?.devPort;
  if (port !== undefined) {
    const browser = yield* closeDevServerBrowserSessions(slug, port);
    if (browser.sessions.length > 0) {
      log.event.info(`closed browser session ${browser.sessions.join(", ")} (${slug})`);
    }
    if (browser.tabs > 0) {
      const s = browser.tabs === 1 ? "" : "s";
      log.event.info(`closed ${browser.tabs} browser tab${s} on port ${port} (${slug})`);
    }
  }
  return stopped;
});


// ---------------------------------------------------------------------------
// Readiness
// ---------------------------------------------------------------------------

/**
 * How long `--wait` gives a server to become usable once it holds a
 * slot. Generous on purpose: the environments this exists for bring up
 * a container stack, apply migrations and seed data before serving, and
 * a first start can genuinely run minutes.
 */
export const DEV_READY_DEFAULT_TIMEOUT_MS = 10 * 60_000;
const DEV_READY_POLL_MS = 2_000;
/**
 * Bound on one `health_command` run. Sized off a measured 9s
 * `docker exec psql` against a live stack, with room for a loaded
 * machine; a hung check must never become a hung `wt dev status`.
 */
const DEV_HEALTH_TIMEOUT_MS = 60_000;

export type DevHealth = {
  ok: boolean;
  /** First line of the command's output, or a description of how it failed. */
  message: string;
};

/**
 * Ask the project whether the environment is actually usable. Null when
 * no `health_command` is configured — which is the honest answer, and
 * distinct from "healthy": callers must not render an unconfigured
 * project as verified.
 *
 * Exit 0 is healthy. Anything else is a problem, and the first line of
 * stdout (falling back to stderr) is the message, so the project owns
 * the wording — wt has no idea what a migration ledger is.
 */
export const devHealth = Effect.fn("devHealth")(function* (wt: {
  slug: string;
  path: string;
}) {
  const template = config.devServer?.healthCommand ?? null;
  const command = resolveTeardownCommand(template, {
    path: wt.path,
    slug: wt.slug,
    port: readWtState().slugs[wt.slug]?.devPort ?? null,
  });
  if (!command) return null;
  const port = readWtState().slugs[wt.slug]?.devPort;
  const r = yield* run([process.env.SHELL || "bash", "-lc", command], {
    cwd: wt.path,
    timeoutMs: DEV_HEALTH_TIMEOUT_MS,
    env: port !== undefined ? { PORT: String(port) } : undefined,
  });
  const firstLine = (text: string) =>
    text
      .split("\n")
      .map((l) => l.trim())
      .find((l) => l !== "") ?? "";
  if (r.exitCode === 0) {
    return { ok: true, message: firstLine(r.stdout) || "healthy" };
  }
  return {
    ok: false,
    message:
      firstLine(r.stdout) ||
      firstLine(r.stderr) ||
      `health_command exited ${r.exitCode}`,
  };
});

export type DevReadyOutcome =
  | { ready: true; health: DevHealth | null }
  /** The supervisor parked after repeated rapid failures. */
  | { ready: false; reason: "crashed" }
  /** The port never opened inside the bound. */
  | { ready: false; reason: "timeout" }
  /** Serving, but the project says the environment is wrong. */
  | { ready: false; reason: "unhealthy"; health: DevHealth };

/**
 * Wait for a just-started dev server to become usable, which is a
 * different question from the one `wt dev start` used to answer.
 *
 * Starting a dev server is asynchronous by construction — wt launches a
 * supervised process and returns — so the exit code carried no
 * information about whether the thing came up. That is not a cosmetic
 * gap: a dev command that starts a database and THEN applies migrations
 * can fail its migration phase minutes after wt reported success,
 * leaving a serviceable stack on a stale schema, a green exit code and
 * a working URL. Two worktrees read that as a healthy environment and
 * one of them reported a passing test suite as broken.
 *
 * Three answers, and they want different next actions: `crashed` (the
 * supervisor gave up — read the logs), `timeout` (still booting, or
 * wedged — read the logs), and `unhealthy` (serving, but the project's
 * own check says the environment is wrong — usually rebuild).
 */
type DevReadyDependencies = {
  readonly status: (slug: string, path: string) => Effect.Effect<DevServerStatus, DevServerOperationError>;
  readonly health: (wt: { slug: string; path: string }) => Effect.Effect<DevHealth | null, DevServerOperationError>;
};

const devReadyDependencies: DevReadyDependencies = {
  status: (slug, path) => devServerStatus(slug, { path }).pipe(
    Effect.mapError((cause) => new DevServerOperationError({ slug, operation: "status", cause })),
  ),
  health: (wt) => devHealth(wt).pipe(
    Effect.mapError((cause) => new DevServerOperationError({ slug: wt.slug, operation: "health", cause })),
  ),
};

export type WaitForDevReadyOptions = {
  timeoutMs?: number;
  /**
   * Absolute deadline (epoch ms), for a caller sharing one budget across
   * a preceding `waitForDevSlot` and this wait — see
   * `WaitForDevSlotOptions.deadlineMs`. Takes precedence over
   * `timeoutMs` when set.
   */
  deadlineMs?: number;
  onTick?: (info: { waited: number; state: "starting" | "checking" }) => void;
};

export const waitForDevReady = Effect.fn("waitForDevReady")(function* (
  wt: { slug: string; path: string },
  opts: WaitForDevReadyOptions = {},
  dependencies: DevReadyDependencies = devReadyDependencies,
): Effect.fn.Return<DevReadyOutcome, DevServerOperationError> {
  const started = yield* Clock.currentTimeMillis;
  const deadline = opts.deadlineMs ?? started + (opts.timeoutMs ?? DEV_READY_DEFAULT_TIMEOUT_MS);
  let lastHealth: DevHealth | null = null;
  const poll = (): Effect.Effect<DevReadyOutcome, DevServerOperationError> =>
    Effect.gen(function* () {
      const st = yield* dependencies.status(wt.slug, wt.path);
      if (st.crashed) return { ready: false, reason: "crashed" };
      if (st.running) {
        const health = yield* dependencies.health(wt);
        if (!health || health.ok) return { ready: true, health };
        // An unhealthy answer is RETRIED rather than believed, because
        // "not yet" and "wrong" are the same answer from a check that
        // runs once. A migration replay in progress reads 29 of 35
        // applied — indistinguishable from a stale volume stuck at 29 —
        // and it settled at 35 a minute later. Somebody encoding this
        // check per-agent has to remember to wait for quiescence, and
        // the report that reached us was written by someone who did not.
        // So the waiting happens here, once, for everyone.
        lastHealth = health;
      }
      const now = yield* Clock.currentTimeMillis;
      if (now >= deadline) {
        return lastHealth
          ? { ready: false, reason: "unhealthy", health: lastHealth }
          : { ready: false, reason: "timeout" };
      }
      opts.onTick?.({ waited: now - started, state: st.running ? "checking" : "starting" });
      yield* Effect.sleep(Duration.millis(DEV_READY_POLL_MS));
      return yield* Effect.suspend(poll);
    });
  return yield* poll();
});

/**
 * Stop the server, run the project's destructive teardown, start it
 * again. The recovery for an environment whose cached state no longer
 * matches the tree — after a rebase above all, where a database keeps
 * the schema it was migrated to while the migration files move
 * underneath it.
 *
 * Exists as a command because the alternative was folklore: the working
 * incantation involved a raw `docker volume rm` that nobody could
 * discover from wt, and the obvious in-place repairs make it worse
 * (`supabase migration up` refuses when newly-arrived stamps sort
 * before the last applied one; `supabase db reset` wipes buckets that
 * are provisioned at start rather than by migrations).
 */
export const resetDevServer = Effect.fn("resetDevServer")(function* (
  wt: { slug: string; path: string },
  onLog?: (line: string) => void,
) {
  const stopped = yield* stopDevServer(wt, onLog);
  // `reset_command` DISCARDS the environment's state (volumes, caches,
  // a migrated database). Doing that on top of an environment that is
  // still up is worse than not resetting at all: whatever survived the
  // failed stop keeps running against state that just vanished, and the
  // rebuild then fails in a way that reads as a broken tree rather than
  // a failed teardown. wt supervises a PROCESS — the thing holding that
  // state is usually not its child — so a failed `stop_command` leaves
  // its external resources unconfirmed, even when the supervisor is gone.
  //
  // The asymmetry with a destroy is deliberate: refusing to delete a
  // worktree because its teardown broke is a bigger leak than the one
  // it prevents, but refusing to DISCARD STATE is the safe direction —
  // the reset command has not run, even if the stop hook partially tore
  // down external resources before failing.
  if (!stopped) {
    return yield* Effect.fail(new DevResetStopFailedError(wt.slug));
  }
  const command = resolveTeardownCommand(config.devServer?.resetCommand ?? null, {
    path: wt.path,
    slug: wt.slug,
    port: readWtState().slugs[wt.slug]?.devPort ?? null,
  });
  if (command) {
    yield* runTeardownCommand({
      label: "reset_command",
      command,
      cwd: wt.path,
      slug: wt.slug,
      onLog: onLog ?? ((line) => log.info(line, { slug: wt.slug })),
    });
  }
  return yield* startDevServer(wt);
});

/**
 * Level-derived state: session existence (tmux) + recorded-port
 * liveness (TCP probe) + the supervisor's marker. Cheap and local —
 * safe to poll from a query.
 *
 * `opts.sessionExists`, when provided, skips the per-slug
 * `tmux has-session` spawn — the caller already knows (from the
 * batched `tmuxSessionsQuery`'s `dev` set) whether the session is
 * live. Omit it (CLI callers with no query cache to read) to fall
 * back to the direct tmux check.
 */
export const devServerStatus = Effect.fn("devServerStatus")(function* (
  slug: string,
  opts: { sessionExists?: boolean; path?: string } = {},
) {
  if (!config.devServer) return DEV_SERVER_STOPPED;
  const port = readWtState().slugs[slug]?.devPort ?? null;
  const session = sessionName(slug, "dev");
  // The supervisor rewrites the marker at the top of every loop pass,
  // so its mtime is when the CURRENT attempt began — not when the
  // server was first asked for. That's the number `starting` wants.
  const since = markerMtime(slug);
  const queued = readDevWaiters();
  const rank = queued.findIndex((w) => w.slug === slug);
  const waiting =
    rank >= 0 ? { rank, since: queued[rank]!.since } : null;
  // One `git merge-base --is-ancestor` (0.1s), and only when there is
  // both an anchor and a path to ask with. Absent reads as null —
  // unknown, never "fine": a server started by a wt that predates the
  // anchor has nothing to compare against, and claiming freshness for
  // it would be the same silent lie the field exists to break.
  const startedSha = readWtState().slugs[slug]?.devStartedSha;
  const rebasedSince =
    startedSha && opts.path
      ? !(yield* shaIsAncestor(startedSha, "HEAD", opts.path))
      : null;
  const base = { port, since, waiting, rebasedSince, restarts: readAttempts(slug) };
  const has =
    opts.sessionExists !== undefined
      ? opts.sessionExists
      : (yield* run(["tmux", "-L", TMUX_SOCKET, "has-session", "-t", `=${session}`]))
          .exitCode === 0;
  if (!has) {
    // No session, but a crashed marker survives (e.g. the pane was lost
    // to a tmux server restart): "it crashed the last time it ran" is
    // still the honest state until a start/stop rewrites it.
    if (readMarker(slug) === "crashed") {
      return { running: false, starting: false, crashed: true, url: null, ...base };
    }
    return { ...DEV_SERVER_STOPPED, ...base };
  }
  const probe = port !== null ? yield* probePort(port) : "free";
  if (probe === "listening") {
    return { running: true, starting: false, crashed: false, url: devUrl(port!), ...base };
  }
  const marker = readMarker(slug);
  if (probe === "unknown" && marker === "running") {
    // Live session, supervisor's last word was "running", and the probe
    // came back with no answer at all. That is not evidence the server
    // went away — reporting it as stopped drops the bolt off the row and
    // makes `s` refuse to open a URL that works. Keep the last known
    // truth; the next pass re-probes.
    return { running: true, starting: false, crashed: false, url: devUrl(port!), ...base };
  }
  if (marker === "crashed") {
    return { running: false, starting: false, crashed: true, url: null, ...base };
  }
  // Session up, port not answering, not parked: either still booting or
  // a stopped-but-remained pane. The marker disambiguates.
  return {
    running: false,
    starting: marker === "running",
    crashed: false,
    url: null,
    ...base,
  };
});
