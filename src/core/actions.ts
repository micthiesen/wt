/**
 * Per-worktree `claude -p` action runner.
 *
 * Action logs live under `<logDir>/actions/<slug>-<iso>.log` — a
 * dedicated subdir, separate from the destroy/tail namespace
 * `latestLogFor` scans (`<logDir>/<slug>-*.log`). Without that split,
 * `wt logs <slug>` would surface action logs the moment one becomes
 * the most-recently-mtimed file for the slug.
 *
 * Stream-json drains race against `proc.exited`; awaitExit awaits
 * both before deciding whether the result event already finalized
 * the run. Without that join, a `result` event still in stdout's
 * pipe at exit produces a duplicate exit line because awaitExit
 * fires its fallback before consumeStdout processes the final byte.
 */
import { mkdirSync } from "node:fs";
import { appendFile } from "node:fs/promises";
import { join } from "node:path";

import {
  type ActionLine,
  type ActionLineKind,
  type ToolStartMap,
  MAX_BUFFERED_LINES,
  asObj,
  formatDuration,
  formatTokens,
  messageToLines,
} from "./claude-events.ts";
import { type ActionDef, type EffectTag, type RequireTag, config } from "./config.ts";
import { createLogger } from "./logger.ts";
import { streamLines } from "./proc.ts";
import type { PullRequest } from "./types.ts";

export type { ActionDef, EffectTag, RequireTag } from "./config.ts";
export type { ActionLine, ActionLineKind } from "./claude-events.ts";

/**
 * Snapshot of row state the requirement predicates read. Kept narrow
 * so callers can pass a row from the TUI aggregator or a synthesized
 * shape from a one-off context without dragging in `WorktreeRow`'s
 * full surface.
 */
export type ActionRowState = {
  pr: PullRequest | undefined;
};

export type ActionAvailability =
  | { ok: true }
  | { ok: false; reason: string };

/**
 * Evaluate a def's `requires` against a row snapshot. Pure synchronous
 * fn: predicates read cached row state, so optimistic patches that
 * already mutated row state cascade for free (mark a PR ready → next
 * picker open shows `requires = ["pr.ready"]` actions as available
 * before the server confirms; rollback re-blocks them). See the
 * architecture block in `state/hooks.ts`.
 */
export function evaluateActionRequirements(
  requires: readonly RequireTag[],
  row: ActionRowState,
): ActionAvailability {
  for (const req of requires) {
    if (req === "pr") {
      if (!row.pr) return { ok: false, reason: "no PR" };
    } else if (req === "pr.ready") {
      if (!row.pr) return { ok: false, reason: "no PR" };
      if (row.pr.isDraft) return { ok: false, reason: "PR is draft" };
      if (row.pr.state !== "OPEN") return { ok: false, reason: "PR not open" };
    }
  }
  return { ok: true };
}

const log = createLogger("[actions]");

/**
 * Per-launch substitution map for action templates. Keys are bare names
 * (no braces); the renderer wraps them as `{{name}}` when scanning.
 *
 * Currently produced at `tui/app.tsx`'s `launchAction` from the row +
 * config. Kept loose (`Record<string, string>`) so adding a new var is
 * a one-liner at the callsite — no schema dance.
 */
export type ActionVars = Readonly<Record<string, string>>;

/**
 * Sed-style template renderer. Replaces `{{name}}` with `vars[name]`;
 * unknown vars pass through unchanged so a typo is visible in the
 * launched prompt (and in the action log header) rather than silently
 * collapsing to an empty string.
 */
export function applyVars(template: string, vars: ActionVars): string {
  return template.replaceAll(/\{\{(\w+)\}\}/g, (m, k) => vars[k] ?? m);
}

/**
 * Window during which a finished run keeps auto-focusing the bottom
 * pane in "follow selected row" mode. Exported so `useActionVisible`
 * reuses the same constant — it drives both the registry-side
 * `isVisible` predicate and the client-side timer, and they have to
 * stay in lockstep. After this window the run drops out of auto-
 * focus, but stays in memory (`MAX_RETAINED_RUNS`) so the Outputs
 * picker can still surface it as a "done"/"failed"/"killed" entry.
 */
export const RECENT_WINDOW_MS = 10 * 1000;
/**
 * How many completed runs to keep in the in-memory registry before
 * evicting the oldest. Drives the Outputs picker — bigger means more
 * historical runs visible without restart, but more memory held.
 * Restart re-hydrates whatever's still on disk via the per-run log.
 */
export const MAX_RETAINED_RUNS = 20;
/** actionId stamped on runs launched via the picker's "Custom prompt…" entry. */
export const CUSTOM_ACTION_ID = "__custom__";

export type ActionStatus = "running" | "succeeded" | "failed" | "killed";

export type ActionRun = {
  slug: string;
  /** Mirrors `ActionDef["kind"]`; used by the runner to pick stream
   *  consumers and process-group-aware kill behavior. */
  kind: "claude" | "shell";
  actionId: string;
  actionName: string;
  prompt: string;
  startedAt: number;
  endedAt?: number;
  status: ActionStatus;
  lines: readonly ActionLine[];
  logPath: string;
  numTurns?: number;
  tokensIn?: number;
  tokensOut?: number;
  /**
   * State domains this run mutates. Snapshot of the def's `affects` at
   * start time so a later config edit can't change which invalidations
   * fire for an in-flight run. Consumed by the TUI subscriber that
   * dispatches cache invalidations when the run reaches a terminal
   * status — see the architecture block in `state/hooks.ts`.
   */
  affects: readonly EffectTag[];
};

export type ActionStartResult =
  | { ok: true; run: ActionRun }
  | { ok: false; reason: string };

type Listener = () => void;

class ActionRegistry {
  private runs: ReadonlyMap<string, ActionRun> = new Map();
  private procs = new Map<string, Bun.Subprocess>();
  /** Per-slug map of tool_use_id → call metadata. Per-slug isolation prevents the
   *  unlikely but real case of two concurrent claude sessions colliding on a
   *  shared id when results would otherwise route to the wrong run. */
  private toolStarts = new Map<string, ToolStartMap>();
  /** Slugs whose `result` event has already been processed. Set inside the
   *  stream parser; read by awaitExit to decide whether to synthesize a
   *  fallback exit line. Cheaper than scanning `lines.at(-1)`, and immune
   *  to a late stderr line shifting `lastKind`. */
  private resultEventSeen = new Set<string>();
  /** stdout+stderr drain promise per slug. awaitExit awaits this before
   *  inspecting `resultEventSeen` so a result event still in the pipe at
   *  exit gets processed before the fallback fires. */
  private drains = new Map<string, Promise<void>>();
  private listeners = new Set<Listener>();
  private cleanupTimer: Timer | null = null;
  /** One write chain per slug — serializes appendFile so log lines never interleave. */
  private writeChains = new Map<string, Promise<void>>();

  start(
    def: ActionDef,
    slug: string,
    cwd: string,
    extras: string,
    vars: ActionVars = {},
  ): ActionStartResult {
    const existing = this.runs.get(slug);
    if (existing?.status === "running") {
      return { ok: false, reason: "an action is already running for this worktree" };
    }

    const actionsDir = join(config.paths.logDir, "actions");
    try {
      mkdirSync(actionsDir, { recursive: true });
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      return { ok: false, reason: `mkdir log dir: ${msg}` };
    }
    const logPath = join(
      actionsDir,
      `${slug}-${new Date().toISOString().replace(/[:.]/g, "-")}.log`,
    );

    const renderedExtras = applyVars(extras, vars);
    const userShell = process.env.SHELL || "bash";
    const argv =
      def.kind === "shell"
        ? [userShell, "-lc", applyVars(def.shell, vars)]
        : (() => {
            const renderedPrompt = applyVars(def.prompt, vars);
            const trimmed = renderedExtras.trim();
            const fullPrompt = trimmed ? `${renderedPrompt}\n\n${trimmed}` : renderedPrompt;
            return [
              "claude",
              "-p",
              "--permission-mode",
              "auto",
              "--verbose",
              "--output-format",
              "stream-json",
              fullPrompt,
            ];
          })();
    const promptForRun =
      def.kind === "shell"
        ? applyVars(def.shell, vars)
        : (() => {
            const renderedPrompt = applyVars(def.prompt, vars);
            const trimmed = renderedExtras.trim();
            return trimmed ? `${renderedPrompt}\n\n${trimmed}` : renderedPrompt;
          })();

    let proc: Bun.Subprocess;
    try {
      proc = Bun.spawn(argv, {
        cwd,
        stdin: "ignore",
        stdout: "pipe",
        stderr: "pipe",
        // Shell actions get their own process group so SIGTERM can be
        // sent to the group (`-pid`) and reach every child bash spawned
        // (`pnpm`, `node`, build subprocesses). Without this, killing
        // bash leaves orphaned children running.
        ...(def.kind === "shell" ? { detached: true } : {}),
      });
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      const target = def.kind === "shell" ? "shell" : "claude";
      return { ok: false, reason: `spawn ${target}: ${msg}` };
    }

    const startTs = Date.now();
    const initialLine: ActionLine = {
      ts: startTs,
      kind: "info",
      text: `▶ ${def.name} starting`,
    };
    const run: ActionRun = {
      slug,
      kind: def.kind,
      actionId: def.id,
      actionName: def.name,
      prompt: promptForRun,
      startedAt: startTs,
      status: "running",
      lines: [initialLine],
      logPath,
      affects: def.affects,
    };
    this.commit((m) => m.set(slug, run));
    this.appendLogFile(run, initialLine);
    log.event.info(`${slug}: ${def.name} → ${logPath}`);

    this.procs.set(slug, proc);
    // toolStarts is a claude-only concept (tracks tool_use_id timing); skip
    // the allocation for shell runs — awaitExit's unconditional delete is a
    // no-op when there's no entry.
    if (def.kind === "claude") this.toolStarts.set(slug, new Map());
    const drain =
      def.kind === "shell"
        ? Promise.all([
            this.consumeShellStream(slug, proc, "stdout"),
            this.consumeShellStream(slug, proc, "stderr"),
          ]).then(() => {})
        : Promise.all([
            this.consumeStdout(slug, proc),
            this.consumeStderr(slug, proc),
          ]).then(() => {});
    this.drains.set(slug, drain);
    void this.awaitExit(slug, proc, drain);
    this.scheduleCleanup();
    return { ok: true, run };
  }

  startCustom(
    slug: string,
    cwd: string,
    prompt: string,
    vars: ActionVars = {},
  ): ActionStartResult {
    return this.start(
      {
        kind: "claude",
        id: CUSTOM_ACTION_ID,
        name: "Custom prompt",
        prompt: "",
        affects: ["git", "github"],
        requires: [],
      },
      slug,
      cwd,
      prompt,
      vars,
    );
  }

  kill(slug: string): boolean {
    const proc = this.procs.get(slug);
    const run = this.runs.get(slug);
    if (!proc || !run || run.status !== "running") return false;
    this.update(slug, (r) => ({ ...r, status: "killed" }));
    try {
      // Shell runs were spawned `detached`, so the bash leader and its
      // children share a pgid equal to the bash pid. Signaling `-pid`
      // hits the whole group; without this, `pnpm`/`node`/build children
      // outlive the bash leader and the user has no way to clean them up.
      if (run.kind === "shell" && proc.pid) {
        process.kill(-proc.pid, "SIGTERM");
      } else {
        proc.kill("SIGTERM");
      }
    } catch {
      // proc might have already exited between the check and the kill —
      // awaitExit will record the final state regardless.
    }
    return true;
  }

  get(slug: string): ActionRun | null {
    return this.runs.get(slug) ?? null;
  }

  /** Returns true while the run for this slug should occupy the activity pane. */
  isVisible(slug: string, now = Date.now()): boolean {
    const run = this.runs.get(slug);
    if (!run) return false;
    if (run.status === "running") return true;
    return run.endedAt !== undefined && now - run.endedAt < RECENT_WINDOW_MS;
  }

  getSnapshot = (): ReadonlyMap<string, ActionRun> => this.runs;

  subscribe = (fn: Listener): (() => void) => {
    this.listeners.add(fn);
    return () => {
      this.listeners.delete(fn);
    };
  };

  /**
   * SIGTERM every running process and await their drains + queued log
   * writes. Returns once everything has settled, so the caller can
   * sequence it before `flushLogger()` and `process.exit`. Doesn't
   * reject — cleanup must always run even if a drain throws.
   */
  async shutdown(): Promise<void> {
    for (const [slug, proc] of this.procs) {
      try {
        const run = this.runs.get(slug);
        if (run?.kind === "shell" && proc.pid) {
          process.kill(-proc.pid, "SIGTERM");
        } else {
          proc.kill("SIGTERM");
        }
      } catch {
        // best-effort
      }
    }
    if (this.cleanupTimer) clearTimeout(this.cleanupTimer);
    this.cleanupTimer = null;
    const pending = [
      ...Array.from(this.drains.values()),
      ...Array.from(this.writeChains.values()),
    ];
    await Promise.allSettled(pending);
  }

  // ---------- internals ----------

  private notify(): void {
    for (const l of this.listeners) {
      try {
        l();
      } catch {
        // sink errors must not break dispatch
      }
    }
  }

  private commit(mut: (m: Map<string, ActionRun>) => void): void {
    const next = new Map(this.runs);
    mut(next);
    this.runs = next;
    this.notify();
  }

  private update(slug: string, mut: (r: ActionRun) => ActionRun): void {
    const cur = this.runs.get(slug);
    if (!cur) return;
    const next = mut(cur);
    this.commit((m) => m.set(slug, next));
  }

  private pushLine(slug: string, line: ActionLine): void {
    const cur = this.runs.get(slug);
    if (!cur) return;
    const lines = cur.lines.length >= MAX_BUFFERED_LINES
      ? [...cur.lines.slice(-(MAX_BUFFERED_LINES - 1)), line]
      : [...cur.lines, line];
    this.commit((m) => m.set(slug, { ...cur, lines }));
    this.appendLogFile(cur, line);
  }

  private appendLogFile(run: ActionRun, line: ActionLine): void {
    const slug = run.slug;
    const path = run.logPath;
    const text = formatLogLine(line);
    const cur = this.writeChains.get(slug) ?? Promise.resolve();
    const next = cur
      .then(() => appendFile(path, `${text}\n`, "utf8"))
      .catch((err) => {
        log.warn("action log write failed", {
          slug,
          path,
          err: err instanceof Error ? err.message : String(err),
        });
      });
    this.writeChains.set(slug, next);
  }

  private async consumeStdout(slug: string, proc: Bun.Subprocess): Promise<void> {
    const stdout = proc.stdout as ReadableStream<Uint8Array> | undefined;
    if (!stdout) return;
    try {
      await streamLines(stdout, (raw) => {
        if (raw.trim()) this.handleStreamLine(slug, raw);
      });
    } catch {
      // Stream error — awaitExit still records final state.
    }
  }

  private async consumeStderr(slug: string, proc: Bun.Subprocess): Promise<void> {
    const stderr = proc.stderr as ReadableStream<Uint8Array> | undefined;
    if (!stderr) return;
    try {
      await streamLines(stderr, (line) => {
        if (line.trim()) {
          this.pushLine(slug, {
            ts: Date.now(),
            kind: "stderr",
            text: `stderr: ${line}`,
          });
        }
      });
    } catch {
      // best-effort
    }
  }

  /** Plain stdout/stderr line tailer for shell actions. Each non-empty
   *  source line becomes one ActionLine; awaitExit synthesizes the
   *  terminal exit-success / exit-failure line. */
  private async consumeShellStream(
    slug: string,
    proc: Bun.Subprocess,
    which: "stdout" | "stderr",
  ): Promise<void> {
    const stream = proc[which] as ReadableStream<Uint8Array> | undefined;
    if (!stream) return;
    try {
      await streamLines(stream, (line) => {
        if (!line.trim()) return;
        this.pushLine(slug, { ts: Date.now(), kind: which, text: line });
      });
    } catch {
      // best-effort
    }
  }

  private async awaitExit(
    slug: string,
    proc: Bun.Subprocess,
    drain: Promise<void>,
  ): Promise<void> {
    const code = await proc.exited;
    // Wait for stdout/stderr to fully drain so a result event still in
    // the pipe at exit gets recorded before we synthesize a fallback.
    await drain;
    if (this.procs.get(slug) === proc) this.procs.delete(slug);
    const cur = this.runs.get(slug);
    if (!cur) {
      this.toolStarts.delete(slug);
      this.resultEventSeen.delete(slug);
      this.drains.delete(slug);
      return;
    }
    const endedAt = Date.now();
    const finalized = this.resultEventSeen.has(slug);
    const status: ActionStatus =
      cur.status === "killed"
        ? "killed"
        : code === 0
          ? "succeeded"
          : "failed";
    if (finalized) {
      this.update(slug, (r) => ({ ...r, endedAt, status }));
    } else {
      const dur = formatDuration(endedAt - cur.startedAt);
      const text =
        status === "killed"
          ? `■ killed after ${dur}`
          : code === 0
            ? `✓ exited 0 in ${dur}`
            : `✗ exited ${code} in ${dur}`;
      const kind: ActionLineKind =
        status === "succeeded" ? "exit-success" : "exit-failure";
      const exitLine: ActionLine = { ts: endedAt, kind, text };
      // Single commit — status flip and exit line in one snapshot so
      // observers don't see a transient frame with the new status but
      // no exit line.
      const lines = cur.lines.length >= MAX_BUFFERED_LINES
        ? [...cur.lines.slice(-(MAX_BUFFERED_LINES - 1)), exitLine]
        : [...cur.lines, exitLine];
      this.commit((m) => m.set(slug, { ...cur, endedAt, status, lines }));
      this.appendLogFile({ ...cur, endedAt, status, lines }, exitLine);
    }
    const dur = formatDuration(endedAt - cur.startedAt);
    if (status === "succeeded") {
      log.event.ok(`${slug}: ${cur.actionName} succeeded (${dur})`);
    } else if (status === "killed") {
      log.event.warn(`${slug}: ${cur.actionName} killed (${dur})`);
    } else {
      log.event.err(`${slug}: ${cur.actionName} failed (${dur})`);
    }
    this.toolStarts.delete(slug);
    this.resultEventSeen.delete(slug);
    this.drains.delete(slug);
    this.scheduleCleanup();
  }

  private handleStreamLine(slug: string, raw: string): void {
    let evt: unknown;
    try {
      evt = JSON.parse(raw);
    } catch {
      return; // ignore non-JSON noise (shouldn't happen with stream-json)
    }
    const e = asObj(evt);
    if (!e) return;
    const ts = Date.now();
    const t = e.type;
    const toolStarts = this.toolStarts.get(slug);

    if ((t === "assistant" || t === "user") && toolStarts) {
      const lines = messageToLines({
        role: t,
        message: e.message,
        ts,
        toolStarts,
      });
      for (const line of lines) this.pushLine(slug, line);
      return;
    }

    if (t === "result") {
      const isErr = e.is_error === true;
      const subtype = typeof e.subtype === "string" ? e.subtype : null;
      const durMs =
        typeof e.duration_ms === "number" ? e.duration_ms : ts - (this.runs.get(slug)?.startedAt ?? ts);
      const turns = typeof e.num_turns === "number" ? e.num_turns : undefined;
      const usage = asObj(e.usage);
      const tokensIn =
        typeof usage?.input_tokens === "number" ? usage.input_tokens : undefined;
      const tokensOut =
        typeof usage?.output_tokens === "number" ? usage.output_tokens : undefined;
      const tokensStr =
        tokensIn !== undefined || tokensOut !== undefined
          ? ` · ${formatTokens(tokensIn ?? 0)} in / ${formatTokens(tokensOut ?? 0)} out`
          : "";
      const turnsStr =
        turns !== undefined ? ` · ${turns} turn${turns === 1 ? "" : "s"}` : "";
      const head = isErr ? `✗ ${subtype ?? "failed"}` : `✓ exited 0`;
      const text = `${head} in ${formatDuration(durMs)}${tokensStr}${turnsStr}`;
      const exitLine: ActionLine = {
        ts,
        kind: isErr ? "exit-failure" : "exit-success",
        text,
      };
      this.resultEventSeen.add(slug);
      // Single commit so observers see the new metadata + exit line in
      // the same render frame.
      const cur = this.runs.get(slug);
      if (cur) {
        const lines = cur.lines.length >= MAX_BUFFERED_LINES
          ? [...cur.lines.slice(-(MAX_BUFFERED_LINES - 1)), exitLine]
          : [...cur.lines, exitLine];
        this.commit((m) => m.set(slug, { ...cur, numTurns: turns, tokensIn, tokensOut, lines }));
        this.appendLogFile({ ...cur, numTurns: turns, tokensIn, tokensOut, lines }, exitLine);
      }
      return;
    }
    // system / rate_limit_event / unknown — silently skip.
  }

  private scheduleCleanup(): void {
    if (this.cleanupTimer) return;
    this.cleanupTimer = setTimeout(() => {
      this.cleanupTimer = null;
      let changed = false;
      const next = new Map(this.runs);
      // Cap by total non-running count rather than the old 10-second
      // window: completed runs stay visible in the Outputs picker
      // until they're FIFO'd out by newer entries. The bounded set
      // means a long wt session doesn't accumulate dozens of dead
      // entries and the per-run side maps don't leak.
      const finished: Array<{ slug: string; endedAt: number }> = [];
      for (const [slug, run] of next) {
        if (run.status === "running") continue;
        if (run.endedAt !== undefined) {
          finished.push({ slug, endedAt: run.endedAt });
        }
      }
      if (finished.length > MAX_RETAINED_RUNS) {
        finished.sort((a, b) => a.endedAt - b.endedAt);
        const drop = finished.slice(0, finished.length - MAX_RETAINED_RUNS);
        for (const { slug } of drop) {
          next.delete(slug);
          this.writeChains.delete(slug);
          this.toolStarts.delete(slug);
          this.resultEventSeen.delete(slug);
          this.drains.delete(slug);
          changed = true;
        }
      }
      if (changed) {
        this.runs = next;
        this.notify();
      }
      // Reschedule while there's anything left to age out.
      if (next.size > 0) this.scheduleCleanup();
    }, 60 * 1000);
  }
}

function formatLogLine(l: ActionLine): string {
  const time = new Date(l.ts).toISOString();
  return `${time} ${l.kind.padEnd(13)} ${l.text}`;
}

export const actionRegistry = new ActionRegistry();
