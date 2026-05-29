/**
 * Action runner — coordinator over disk + tmux state.
 *
 * # Why tmux supervises actions
 *
 * Action runs are wrapped in a small bash supervisor (see
 * `core/action-tmux.ts`) hosted inside a `<slug>-action` tmux session.
 * The wrapper redirects stdout/stderr to per-run log files and writes
 * a `done.json` sentinel on exit. Because tmux is the wrapper's parent
 * (not wt), action runs survive a wt restart: the next `wt` invocation
 * rehydrates the in-memory `runs` map by reading meta.json and the
 * stream-log seed, and re-attaches a tail. No more "I started a
 * deploy, restarted wt, and lost the action."
 *
 * # Source-of-truth split
 *
 *  - `<runDir>/meta.json` — static metadata (id, name, prompt, kind,
 *    affects, startedAt) plus the final state when terminal
 *    (`endedAt`, `exitCode`, `status`).
 *  - Tmux session existence — "is this run currently executing?"
 *    Polled lazily on boot; the in-memory `runs` map is the
 *    steady-state cache for everything else.
 *  - `<runDir>/stream.log`, `<runDir>/stderr.log` — every emitted
 *    line, monotonic. Tail-seeded on boot; live-watched while running.
 *  - `<runDir>/done.json` — wrapper's exit sentinel. Triggers status
 *    finalization in the live path; consumed by the boot reconciler
 *    for runs that completed while wt was down.
 *  - `actionRegistry.runs` — hot in-memory cache, reconstructable
 *    from disk. Bounded by MAX_RETAINED_RUNS.
 *
 * # Why claude stream-json + shell raw output share this module
 *
 * The wrapper redirects both unchanged: claude `-p --output-format
 * stream-json` writes JSON-per-line to its stdout (redirected to
 * stream.log), shell actions write whatever they write. The tail
 * emits raw lines either way; this module's per-kind parser branch
 * (`handleStreamJsonLine` vs the raw stdout/stderr push) does the
 * right thing for each.
 */
import {
  existsSync,
  mkdirSync,
  readFileSync,
  readdirSync,
  renameSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";

import {
  type ActionLine,
  type ActionLineKind,
  type MessageEmit,
  type ToolStartMap,
  MAX_BUFFERED_LINES,
  asObj,
  formatDuration,
  formatTokens,
  messageToLines,
} from "./claude-events.ts";
import {
  type ActionTailHandle,
  type DoneSentinel,
  type TailLine,
  readDoneFile,
  seedActionDir,
  startActionTail,
  watchDoneSentinel,
} from "./action-tail.ts";
import {
  killActionSession as killActionTmuxSession,
  startActionSession,
} from "./action-tmux.ts";
import {
  DEFAULT_CLAUDE_AFFECTS,
  DEFAULT_REQUIRES,
  type ActionDef,
  type EffectTag,
  type RequireTag,
  config,
} from "./config.ts";
import { createLogger } from "./logger.ts";
import { sanitizeLine } from "./proc.ts";
import { listSessions } from "./tmux.ts";
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
  /**
   * `isOurStageDeployed` result for the row. Strict gate (matches
   * the safe-stage rules); used by `requires: ["deployed"]` actions
   * like the built-in `remove-local`.
   */
  deployed: boolean;
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
    switch (req) {
      case "pr":
        if (!row.pr) return { ok: false, reason: "no PR" };
        break;
      case "pr.ready":
        if (!row.pr) return { ok: false, reason: "no PR" };
        if (row.pr.isDraft) return { ok: false, reason: "PR is draft" };
        if (row.pr.state !== "OPEN") return { ok: false, reason: "PR not open" };
        break;
      case "deployed":
        if (!row.deployed) return { ok: false, reason: "no stage deployed" };
        break;
      default: {
        // Exhaustiveness check, adding a new RequireTag without
        // updating this switch is a type error. Critical because the
        // failure mode is silent always-allow (worse than always-block).
        const _exhaustive: never = req;
        throw new Error(`unhandled require tag: ${String(_exhaustive)}`);
      }
    }
  }
  return { ok: true };
}

/**
 * Built-in shell actions appended after `config.actions` in the
 * picker. They behave exactly like user-configured shell actions —
 * the `actionRegistry` doesn't distinguish, the only difference is
 * they're defined in code rather than read from `config.toml`.
 * Adding one: declare it here, no other wiring needed. The picker
 * places these between user actions and the trailing "Custom prompt…"
 * sentinel.
 */
export const BUILTIN_ACTIONS: readonly ActionDef[] = [
  {
    kind: "shell",
    id: "remove-local",
    name: "Remove local",
    group: "Deploy",
    // `{{stage}}` resolves to the stage this worktree owns — the pinned
    // `.sst/stage` (prefix-guarded) — so remove targets what's actually
    // deployed. The `requires: ["deployed"]` gate runs the same
    // `safeStage` prefix check, so a foreign/unprefixed pin never gets
    // here.
    shell: "pnpm sst remove --stage {{stage}}",
    affects: ["git"],
    requires: ["deployed"],
    argPrompt: null,
    labelExtract: null,
  },
];

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
 * historical runs visible without restart, but more memory held. The
 * boot reconciler also uses this cap when rehydrating from disk.
 */
export const MAX_RETAINED_RUNS = 20;
/** actionId stamped on runs launched via the picker's "Custom prompt…" entry. */
export const CUSTOM_ACTION_ID = "__custom__";

export type ActionStatus = "running" | "succeeded" | "failed" | "killed";

export type ActionRun = {
  slug: string;
  /** Mirrors `ActionDef["kind"]`; used by the parser to decide between
   *  stream-json and raw stdout handling. */
  kind: "claude" | "shell";
  actionId: string;
  actionName: string;
  prompt: string;
  startedAt: number;
  endedAt?: number;
  status: ActionStatus;
  lines: readonly ActionLine[];
  /** Per-run dir under `<logDir>/actions/`. Holds meta.json,
   *  stream.log, stderr.log, and (when terminal) done.json. */
  runDir: string;
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

/** On-disk shape of `meta.json`. Fields beyond the run identity may
 *  be missing in older runs; the loader tolerates that and reconstructs
 *  conservatively. */
type ActionMeta = {
  version: 1;
  slug: string;
  runId: string;
  kind: "claude" | "shell";
  actionId: string;
  actionName: string;
  prompt: string;
  affects: readonly EffectTag[];
  startedAt: number;
  endedAt?: number;
  exitCode?: number;
  status: ActionStatus;
};

type LiveHandles = {
  tail: ActionTailHandle;
  done: ActionTailHandle;
  /** Per-run map of tool_use_id → call metadata. Survives across the
   *  boot-seed → live-tail handoff so result-event durations are
   *  computed against the same start record. */
  toolStarts: ToolStartMap;
  /** True once the stream-json `result` event has been processed for
   *  this run; prevents the done.json handler from synthesizing a
   *  duplicate exit line. */
  resultEventSeen: boolean;
};

class ActionRegistry {
  private runs: ReadonlyMap<string, ActionRun> = new Map();
  /** Per slug: tail + done watcher + parser state for the in-flight run. */
  private liveHandles = new Map<string, LiveHandles>();
  private listeners = new Set<Listener>();
  private cleanupTimer: Timer | null = null;
  /**
   * Registry-global monotonic line id. Every ActionLine emitted by this
   * registry — across all runs, all slugs — pulls from this counter.
   * Lines patched by id are scoped per-slug (the result handler only
   * touches its own slug's buffer), so cross-slug uniqueness isn't
   * required; using one counter just simplifies plumbing — every emit
   * site goes through `nextLineId()`.
   */
  private nextId = 1;
  private nextLineId = (): number => this.nextId++;
  /** Per runDir: serialized meta.json write chain. Concurrent updates
   *  (status flip + result-event metadata) would otherwise race and
   *  one would lose. */
  private metaChains = new Map<string, Promise<void>>();
  /** Slugs with a `start()` in flight but not yet committed to `runs`.
   *  `start()` awaits `startActionSession` now, so the "one running per
   *  slug" guard can no longer rely on check-then-commit being atomic —
   *  a second concurrent start() could slip through the window between
   *  the guard and the `runs` commit. Reserving the slug here keeps the
   *  guard meaningful across that await. */
  private starting = new Set<string>();

  async start(
    def: ActionDef,
    slug: string,
    cwd: string,
    extras: string,
    vars: ActionVars = {},
  ): Promise<ActionStartResult> {
    const existing = this.runs.get(slug);
    if (existing?.status === "running" || this.starting.has(slug)) {
      return { ok: false, reason: "an action is already running for this worktree" };
    }
    // Reserve the slug synchronously across the async `startActionSession`
    // inside `startInner` so a second concurrent start() can't slip past
    // the guard above before this run lands in `runs`.
    this.starting.add(slug);
    try {
      return await this.startInner(def, slug, cwd, extras, vars);
    } finally {
      this.starting.delete(slug);
    }
  }

  private async startInner(
    def: ActionDef,
    slug: string,
    cwd: string,
    extras: string,
    vars: ActionVars,
  ): Promise<ActionStartResult> {
    // `kill()` synchronously closes the prior run's tail + done
    // watcher and tmux-kills the session, so by the time we reach
    // here `liveHandles[slug]` should be empty. Defense-in-depth:
    // if a prior watcher somehow lingers, drop it now to prevent the
    // old wrapper's done.json from finalizing the new run via stale
    // handles installed below.
    const stale = this.liveHandles.get(slug);
    if (stale) {
      try { stale.tail.close(); } catch { /* best-effort */ }
      try { stale.done.close(); } catch { /* best-effort */ }
      this.liveHandles.delete(slug);
    }

    const startedAt = Date.now();
    const runId = formatRunId(slug, startedAt);
    const runDir = join(actionsDir(), runId);
    try {
      mkdirSync(runDir, { recursive: true });
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      return { ok: false, reason: `mkdir run dir: ${msg}` };
    }

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

    const meta: ActionMeta = {
      version: 1,
      slug,
      runId,
      kind: def.kind,
      actionId: def.id,
      actionName: def.name,
      prompt: promptForRun,
      affects: def.affects,
      startedAt,
      status: "running",
    };
    try {
      writeMetaSync(runDir, meta);
    } catch (err) {
      const msg = err instanceof Error ? err.message : String(err);
      return { ok: false, reason: `write meta: ${msg}` };
    }

    const spawnResult = await startActionSession({ slug, cwd, runDir, argv });
    if (!spawnResult.ok) {
      // Persist a failed sentinel so a later boot doesn't see a
      // "running" run with no tmux session.
      writeDoneSentinelBestEffort(runDir, -1);
      void this.persistMetaUpdate(runDir, {
        status: "failed",
        endedAt: Date.now(),
      });
      return { ok: false, reason: spawnResult.reason };
    }

    const initialLine: ActionLine = {
      id: this.nextLineId(),
      ts: startedAt,
      kind: "info",
      text: `▶ ${def.name} starting`,
    };
    const run: ActionRun = {
      slug,
      kind: def.kind,
      actionId: def.id,
      actionName: def.name,
      prompt: promptForRun,
      startedAt,
      status: "running",
      lines: [initialLine],
      runDir,
      affects: def.affects,
    };
    this.commit((m) => m.set(slug, run));
    log.event.info(`${slug}: ${def.name} → ${runDir}`);

    // Live tail with seed=false: the wrapper just opened the log files,
    // there's nothing to seed, and snapping `lastByte` to 0 is correct.
    this.attachLive(slug, runDir, def.kind, false);
    this.scheduleCleanup();
    return { ok: true, run };
  }

  startCustom(
    slug: string,
    cwd: string,
    prompt: string,
    vars: ActionVars = {},
  ): Promise<ActionStartResult> {
    return this.start(
      {
        kind: "claude",
        id: CUSTOM_ACTION_ID,
        name: "Custom prompt",
        prompt: "",
        target: "headless",
        affects: DEFAULT_CLAUDE_AFFECTS,
        requires: DEFAULT_REQUIRES,
        argPrompt: null,
        labelExtract: null,
      },
      slug,
      cwd,
      prompt,
      vars,
    );
  }

  /**
   * Finalize the run as killed. The in-memory teardown — close the tail
   * (its final flush captures any last-second output), append the killed
   * exit-line, commit the `killed` status, persist meta — all runs
   * *synchronously* before the awaited `tmux kill-session`. So
   * fire-and-forget callers (`doRemove`/`doClean`) get the status flip
   * immediately, and a re-entrant kill() sees a terminal run and bails.
   * Closing handles before anything else prevents the old wrapper's
   * done.json from landing on a freshly-installed `liveHandles[slug]`.
   * The awaited tmux kill frees the `<slug>-action` session name once it
   * resolves; the wrapper's EXIT trap then fires and writes done.json,
   * but no one watches it — the run is already terminal here.
   *
   * Caveat: the "running" guard is released at the synchronous status
   * flip, *before* the tmux name is freed. A `start()` interleaved into
   * that window collides on the still-alive session; `new-session` (no
   * `-A`) fails loudly rather than corrupting state — the intended
   * backstop, not dead code.
   */
  async kill(slug: string): Promise<boolean> {
    const run = this.runs.get(slug);
    if (!run || run.status !== "running") return false;

    // Close watchers first; the tail's final-flush sucks any pending
    // log lines into the runs map before we synthesize the exit line
    // so the line ordering matches what the user saw streaming.
    const handles = this.liveHandles.get(slug);
    if (handles) {
      try { handles.tail.close(); } catch { /* best-effort */ }
      try { handles.done.close(); } catch { /* best-effort */ }
      this.liveHandles.delete(slug);
    }

    // Commit the killed status SYNCHRONOUSLY — before the async tmux
    // kill below — for two reasons: callers that fire-and-forget
    // (`doRemove`, `doClean`) still get the immediate UI flip to
    // "killed" they rely on, and a re-entrant kill() during the await
    // sees a terminal run (status !== "running") and bails instead of
    // appending a second exit line.
    const cur = this.runs.get(slug)!;
    const endedAt = Date.now();
    const dur = formatDuration(endedAt - cur.startedAt);
    const exitLine: ActionLine = {
      id: this.nextLineId(),
      ts: endedAt,
      kind: "exit-failure",
      text: `■ killed after ${dur}`,
    };
    const lines = capLines([...cur.lines, exitLine]);
    this.commit((m) =>
      m.set(slug, { ...cur, status: "killed", endedAt, lines }),
    );

    // Persist the killed status synchronously: the wrapper may write
    // done.json after we return, and a wt restart in between would
    // see meta.status="killed" + done.json present and correctly hold
    // the killed status (boot reconciler prefers terminal meta over
    // done.json's exit code).
    try {
      const existing = readMetaSafe(run.runDir);
      if (existing) {
        writeMetaSync(run.runDir, {
          ...existing,
          status: "killed",
          endedAt,
        });
      }
    } catch (err) {
      log.warn("kill meta persist failed", {
        slug,
        runDir: run.runDir,
        err: err instanceof Error ? err.message : String(err),
      });
    }

    // Free the tmux session name so a follow-up start() can reclaim it;
    // awaited last, after the in-memory teardown above. The wrapper's
    // EXIT trap fires once the session dies and writes done.json, but
    // no one watches it — the run is already terminal here.
    await killActionTmuxSession(slug);

    log.event.warn(`${slug}: ${cur.actionName} killed (${dur})`);
    this.scheduleCleanup();
    return true;
  }

  /**
   * Hydrate `runs` from `<logDir>/actions/` and the live tmux session
   * list, then attach live tails for runs still running. Idempotent —
   * safe to call multiple times, though it only makes sense once at
   * startup. Keeps the most-recent MAX_RETAINED_RUNS by directory
   * mtime, dropping older runs from the in-memory cache (their files
   * stay on disk).
   */
  async boot(liveSlugs: ReadonlySet<string>): Promise<void> {
    const dir = actionsDir();
    if (!existsSync(dir)) return;
    let names: string[];
    try {
      names = readdirSync(dir);
    } catch (err) {
      log.warn("boot: read actions dir failed", {
        err: err instanceof Error ? err.message : String(err),
      });
      return;
    }
    const candidates: { name: string; mtime: number }[] = [];
    for (const name of names) {
      const path = join(dir, name);
      try {
        const st = statSync(path);
        if (!st.isDirectory()) continue;
        candidates.push({ name, mtime: st.mtimeMs });
      } catch {
        // skip unreadable entries
      }
    }
    candidates.sort((a, b) => b.mtime - a.mtime);
    const keep = candidates.slice(0, MAX_RETAINED_RUNS);
    if (keep.length === 0) return;

    const sessions = await listSessions();
    const liveActionSlugs = sessions.action;

    let restored = 0;
    let orphans = 0;
    for (const { name } of keep) {
      const runDir = join(dir, name);
      const meta = readMetaSafe(runDir);
      if (!meta) continue;
      // Drop runs whose slug no longer exists. The session reaper kills
      // their tmux session at startup; we mirror by not surfacing the
      // run in the picker. Files stay on disk for one more reap pass.
      if (!liveSlugs.has(meta.slug) && !liveActionSlugs.has(meta.slug)) {
        continue;
      }
      const done = readDoneSafe(runDir);
      const sessionExists = liveActionSlugs.has(meta.slug);

      // Reconcile status. Branches:
      //  - meta already terminal (succeeded/failed/killed) → use as-is.
      //    `kill()` persists `killed` synchronously, so a kill-before-
      //    restart correctly stays killed regardless of what the
      //    wrapper's done.json says.
      //  - status=running + done.json present → wrapper finished while
      //    wt was down; classify by exit code.
      //  - status=running + no session + no done.json → orphan
      //    (wrapper crashed bypass-trap or external `kill -9`); mark
      //    failed and write a sentinel so the file shape stays
      //    consistent across reboots.
      //  - status=running + session present → live; will attach below.
      let metaResolved = meta;
      if (meta.status === "running") {
        if (done) {
          const status: ActionStatus =
            done.exitCode === 0 ? "succeeded" : "failed";
          metaResolved = {
            ...meta,
            status,
            endedAt: done.endedAt,
            exitCode: done.exitCode,
          };
          void this.persistMetaUpdate(runDir, {
            status,
            endedAt: done.endedAt,
            exitCode: done.exitCode,
          });
        } else if (!sessionExists) {
          orphans++;
          const endedAt = Date.now();
          metaResolved = { ...meta, status: "failed", endedAt };
          // Write a sentinel so future boots see the same "completed"
          // file shape as a normally-terminated run; -1 marks "exit
          // code unknown / wrapper bypassed its EXIT trap".
          writeDoneSentinelBestEffort(runDir, -1);
          void this.persistMetaUpdate(runDir, { status: "failed", endedAt });
        }
      }

      // Seed lines from disk via the same parser the live tail uses.
      // For terminal runs this is the only read; for running runs the
      // live tail will pick up where the seed leaves off.
      const handles = makeFreshHandles();
      // Synthesize the ▶ start header that `start()` injects into the
      // in-memory line buffer (it's not in stream.log because the
      // wrapper redirects after launch, not before). Keeps the
      // rehydrated rendering visually consistent with a freshly-
      // launched run.
      let seededLines: ActionLine[] = [
        {
          id: this.nextLineId(),
          ts: metaResolved.startedAt,
          kind: "info",
          text: `▶ ${metaResolved.actionName} starting`,
        },
      ];
      seedActionDir({
        runDir,
        onLine: (line) => {
          const emit = this.parseLine(metaResolved.kind, line, handles);
          seededLines = applyEmit(seededLines, emit);
        },
      });

      const run = materializeRun(metaResolved, runDir, capLines(seededLines));
      this.commit((m) => m.set(meta.slug, run));
      restored++;

      if (run.status === "running") {
        // Carry the seed-built handles into live mode so result-event
        // timings span the boot boundary correctly.
        this.attachLiveWithHandles(meta.slug, runDir, metaResolved.kind, handles);
      }
    }

    if (restored > 0 || orphans > 0) {
      log.info("boot rehydrated runs", { restored, orphans });
    }
    this.scheduleCleanup();
  }

  /**
   * Drop on-disk run dirs that won't be rehydrated and whose slug is
   * gone. Called from startup reap so `<logDir>/actions/` doesn't
   * grow without bound. Two cuts:
   *  - any run dir whose slug isn't in `liveSlugs` (the worktree was
   *    destroyed; the in-memory run for it would've been dropped at
   *    boot anyway).
   *  - any run dir beyond the newest MAX_RETAINED_RUNS by mtime —
   *    these will never appear in the picker again.
   *
   * Only runs whose meta status is terminal are reaped; a still-
   * running run is left alone regardless of slug membership so a
   * race-y reap never deletes a wrapper's working directory mid-
   * write. Errors are swallowed.
   */
  reapDirs(liveSlugs: ReadonlySet<string>): void {
    const dir = actionsDir();
    if (!existsSync(dir)) return;
    let names: string[];
    try {
      names = readdirSync(dir);
    } catch {
      return;
    }
    type Entry = { name: string; mtime: number; slug: string; terminal: boolean };
    const entries: Entry[] = [];
    for (const name of names) {
      const path = join(dir, name);
      try {
        const st = statSync(path);
        if (!st.isDirectory()) continue;
        const meta = readMetaSafe(path);
        if (!meta) continue;
        entries.push({
          name,
          mtime: st.mtimeMs,
          slug: meta.slug,
          terminal: meta.status !== "running",
        });
      } catch {
        // skip
      }
    }
    entries.sort((a, b) => b.mtime - a.mtime);
    let removed = 0;
    for (let i = 0; i < entries.length; i++) {
      const e = entries[i]!;
      if (!e.terminal) continue;
      const dropForSlug = !liveSlugs.has(e.slug);
      const dropForAge = i >= MAX_RETAINED_RUNS;
      if (!dropForSlug && !dropForAge) continue;
      const path = join(dir, e.name);
      try {
        rmSync(path, { recursive: true, force: true });
        removed++;
      } catch (err) {
        log.warn("action dir reap failed", {
          path,
          err: err instanceof Error ? err.message : String(err),
        });
      }
    }
    if (removed > 0) log.info("reaped action dirs", { removed });
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
   * Stop watchers, release handles, await pending meta writes. Does
   * NOT kill tmux sessions — the whole point of the supervisor design
   * is that running actions outlive wt restarts. The next `wt`
   * invocation rehydrates them via `boot`.
   */
  async shutdown(): Promise<void> {
    for (const handles of this.liveHandles.values()) {
      try {
        handles.tail.close();
      } catch {
        // best-effort
      }
      try {
        handles.done.close();
      } catch {
        // best-effort
      }
    }
    this.liveHandles.clear();
    if (this.cleanupTimer) clearTimeout(this.cleanupTimer);
    this.cleanupTimer = null;
    await Promise.allSettled(this.metaChains.values());
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

  /**
   * Live mode: open the tail + done watcher, install `handles` in the
   * registry. Used both by `start` (fresh handles) and `boot`
   * (handles built up during seeding so toolStarts state crosses the
   * restart boundary).
   */
  private attachLive(
    slug: string,
    runDir: string,
    kind: "claude" | "shell",
    seed: boolean,
  ): void {
    this.attachLiveWithHandles(slug, runDir, kind, makeFreshHandles(), seed);
  }

  private attachLiveWithHandles(
    slug: string,
    runDir: string,
    kind: "claude" | "shell",
    handles: { toolStarts: ToolStartMap; resultEventSeen: boolean },
    seed = false,
  ): void {
    const onLine = (line: TailLine) => {
      const live = this.liveHandles.get(slug);
      if (!live) return;
      const emit = this.parseLine(kind, line, live);
      if (emit.append.length === 0 && emit.patch.length === 0) return;
      const cur = this.runs.get(slug);
      if (!cur) return;
      const lines = capLines(applyEmit(cur.lines, emit));
      this.commit((m) => m.set(slug, { ...cur, lines }));
    };
    const tail = startActionTail({ runDir, onLine, seed });
    const done = watchDoneSentinel({
      runDir,
      onDone: (sentinel) => this.handleDone(slug, sentinel),
    });
    this.liveHandles.set(slug, {
      tail,
      done,
      toolStarts: handles.toolStarts,
      resultEventSeen: handles.resultEventSeen,
    });
  }

  /**
   * Convert one raw tail line into a parser delta. The two branches
   * mirror the per-kind handlers from the pre-tmux runner: claude
   * stdout is stream-json (parsed for tool/result events), shell
   * stdout/stderr and claude stderr are raw text appended directly.
   */
  private parseLine(
    kind: "claude" | "shell",
    line: TailLine,
    handles: { toolStarts: ToolStartMap; resultEventSeen: boolean },
  ): MessageEmit {
    if (kind === "claude" && line.source === "stdout") {
      return this.parseStreamJsonLine(line.text, handles);
    }
    const cleaned = sanitizeLine(line.text);
    if (!cleaned) return { append: [], patch: [] };
    const lineKind: ActionLineKind =
      line.source === "stderr" ? "stderr" : "stdout";
    // Claude stderr is rare (mostly setup errors); render with a
    // `stderr:` prefix for parity with the pre-tmux behavior.
    const text =
      kind === "claude" && line.source === "stderr"
        ? `stderr: ${cleaned}`
        : cleaned;
    return {
      append: [
        { id: this.nextLineId(), ts: Date.now(), kind: lineKind, text },
      ],
      patch: [],
    };
  }

  private parseStreamJsonLine(
    raw: string,
    handles: { toolStarts: ToolStartMap; resultEventSeen: boolean },
  ): MessageEmit {
    if (!raw.trim()) return { append: [], patch: [] };
    let evt: unknown;
    try {
      evt = JSON.parse(raw);
    } catch {
      return { append: [], patch: [] };
    }
    const e = asObj(evt);
    if (!e) return { append: [], patch: [] };
    const ts = Date.now();
    const t = e.type;

    if (t === "assistant" || t === "user") {
      return messageToLines({
        role: t,
        message: e.message,
        ts,
        toolStarts: handles.toolStarts,
        nextId: this.nextLineId,
      });
    }

    if (t === "result") {
      const isErr = e.is_error === true;
      const subtype = typeof e.subtype === "string" ? e.subtype : null;
      const durMs =
        typeof e.duration_ms === "number" ? e.duration_ms : 0;
      const turns = typeof e.num_turns === "number" ? e.num_turns : undefined;
      const usage = asObj(e.usage);
      const tokensIn =
        typeof usage?.input_tokens === "number" ? usage.input_tokens : undefined;
      const tokensOut =
        typeof usage?.output_tokens === "number"
          ? usage.output_tokens
          : undefined;
      const tokensStr =
        tokensIn !== undefined || tokensOut !== undefined
          ? ` · ${formatTokens(tokensIn ?? 0)} in / ${formatTokens(tokensOut ?? 0)} out`
          : "";
      const turnsStr =
        turns !== undefined ? ` · ${turns} turn${turns === 1 ? "" : "s"}` : "";
      const head = isErr ? `✗ ${subtype ?? "failed"}` : "✓ exited 0";
      const text = `${head} in ${formatDuration(durMs)}${tokensStr}${turnsStr}`;
      const exitLine: ActionLine = {
        id: this.nextLineId(),
        ts,
        kind: isErr ? "exit-failure" : "exit-success",
        text,
      };
      handles.resultEventSeen = true;
      // Turns/tokens are encoded inline in the exit line text, so we
      // intentionally don't carry them as separate fields on ActionRun.
      // Adding a panel that wants them later would mean re-extracting
      // from the result event at that point — cheap, since stream.log
      // is on disk.
      return { append: [exitLine], patch: [] };
    }
    return { append: [], patch: [] };
  }

  /**
   * Wrapper has exited; finalize the run. Idempotent — done watcher
   * fires once, but if a kill races a natural completion the second
   * call is a no-op because the run is no longer in `liveHandles`.
   */
  private handleDone(slug: string, done: DoneSentinel): void {
    const handles = this.liveHandles.get(slug);
    if (!handles) return;
    // Close the tail FIRST so its final flush-read appends any
    // last-second lines (the wrapper may have written stdout
    // microseconds before the EXIT trap created done.json) into
    // the run's lines array. Without this, the synthesized exit
    // line lands before the trailing output and a viewer reading
    // the buffer sees `... line-3 [exit-success]` reordered.
    handles.tail.close();
    handles.done.close();
    this.liveHandles.delete(slug);

    const cur = this.runs.get(slug);
    if (!cur) return;

    const status: ActionStatus =
      cur.status === "killed"
        ? "killed"
        : done.exitCode === 0
          ? "succeeded"
          : "failed";
    const endedAt = done.endedAt;

    if (handles.resultEventSeen) {
      // Stream-json result event already emitted the exit line; just
      // flip status + endedAt.
      this.commit((m) =>
        m.set(slug, { ...cur, endedAt, status }),
      );
    } else {
      const dur = formatDuration(endedAt - cur.startedAt);
      const text =
        status === "killed"
          ? `■ killed after ${dur}`
          : done.exitCode === 0
            ? `✓ exited 0 in ${dur}`
            : `✗ exited ${done.exitCode} in ${dur}`;
      const lineKind: ActionLineKind =
        status === "succeeded" ? "exit-success" : "exit-failure";
      const exitLine: ActionLine = {
        id: this.nextLineId(),
        ts: endedAt,
        kind: lineKind,
        text,
      };
      const lines = capLines([...cur.lines, exitLine]);
      this.commit((m) => m.set(slug, { ...cur, endedAt, status, lines }));
    }

    void this.persistMetaUpdate(cur.runDir, {
      status,
      endedAt,
      exitCode: done.exitCode,
    });

    const dur = formatDuration(endedAt - cur.startedAt);
    if (status === "succeeded") {
      log.event.ok(`${slug}: ${cur.actionName} succeeded (${dur})`);
    } else if (status === "killed") {
      log.event.warn(`${slug}: ${cur.actionName} killed (${dur})`);
    } else {
      log.event.err(`${slug}: ${cur.actionName} failed (${dur})`);
    }
    this.scheduleCleanup();
  }

  private persistMetaUpdate(
    runDir: string,
    patch: Partial<ActionMeta>,
  ): Promise<void> {
    const cur = this.metaChains.get(runDir) ?? Promise.resolve();
    const next = cur
      .then(async () => {
        const existing = readMetaSafe(runDir);
        if (!existing) return;
        const merged: ActionMeta = { ...existing, ...patch };
        try {
          writeMetaSync(runDir, merged);
        } catch (err) {
          log.warn("meta write failed", {
            runDir,
            err: err instanceof Error ? err.message : String(err),
          });
        }
      })
      .catch((err) => {
        log.warn("meta chain error", {
          runDir,
          err: err instanceof Error ? err.message : String(err),
        });
      });
    this.metaChains.set(runDir, next);
    return next;
  }

  private scheduleCleanup(): void {
    if (this.cleanupTimer) return;
    this.cleanupTimer = setTimeout(() => {
      this.cleanupTimer = null;
      let changed = false;
      const next = new Map(this.runs);
      // Cap by total non-running count: completed runs stay visible
      // in the Outputs picker until they're FIFO'd out by newer
      // entries. Bounded set keeps memory predictable; the on-disk
      // run dirs survive past eviction so a future boot can rehydrate
      // them if desired.
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
          changed = true;
        }
      }
      if (changed) {
        this.runs = next;
        this.notify();
      }
      if (next.size > 0) this.scheduleCleanup();
    }, 60 * 1000);
  }
}

// ---------- helpers ----------

function actionsDir(): string {
  return join(config.paths.logDir, "actions");
}

/** Filesystem-safe per-run directory id: `<slug>-<iso>` with `:`/`.`
 *  replaced. Stable across reads of the same run; distinct across
 *  runs even on the same slug. */
function formatRunId(slug: string, startedAt: number): string {
  return `${slug}-${new Date(startedAt).toISOString().replace(/[:.]/g, "-")}`;
}

function makeFreshHandles(): {
  toolStarts: ToolStartMap;
  resultEventSeen: boolean;
} {
  return { toolStarts: new Map(), resultEventSeen: false };
}

/**
 * Apply one parser delta to a buffer snapshot. Patches by id (no-op
 * when the id has already been evicted past `MAX_BUFFERED_LINES`),
 * then appends. Cheap at our buffer scale (1000 lines, a handful of
 * patches per delta) and stays pure for reasoning.
 */
function applyEmit(
  prev: readonly ActionLine[],
  emit: MessageEmit,
): ActionLine[] {
  const { append, patch } = emit;
  if (append.length === 0 && patch.length === 0) return prev.slice();
  let next: ActionLine[] = prev.slice();
  if (patch.length > 0) {
    const byId = new Map<number, ActionLine>();
    for (const p of patch) byId.set(p.id, p.line);
    next = next.map((l) => byId.get(l.id) ?? l);
  }
  if (append.length > 0) next = next.concat(append);
  return next;
}

function capLines(lines: readonly ActionLine[]): readonly ActionLine[] {
  return lines.length > MAX_BUFFERED_LINES
    ? lines.slice(-MAX_BUFFERED_LINES)
    : lines;
}

function readMetaSafe(runDir: string): ActionMeta | null {
  const path = join(runDir, "meta.json");
  if (!existsSync(path)) return null;
  try {
    const raw = readFileSync(path, "utf8");
    const parsed = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object") return null;
    // Skip-with-warn on a future schema bump rather than silently
    // treating an unknown shape as v1. Adding fields is fine; field
    // semantics changing is what version bumps would signal.
    if (parsed.version !== 1) {
      log.warn("meta version unsupported", { path, version: parsed.version });
      return null;
    }
    return parsed as ActionMeta;
  } catch (err) {
    log.warn("meta read failed", {
      path,
      err: err instanceof Error ? err.message : String(err),
    });
    return null;
  }
}

function readDoneSafe(runDir: string): DoneSentinel | null {
  return readDoneFile(join(runDir, "done.json"));
}

/** Atomic-ish write: write to a temp sibling and rename. fs.watch
 *  consumers that fire mid-write get a complete object rather than a
 *  half-written one. */
function writeMetaSync(runDir: string, meta: ActionMeta): void {
  const finalPath = join(runDir, "meta.json");
  const tmpPath = `${finalPath}.tmp`;
  writeFileSync(tmpPath, JSON.stringify(meta, null, 2));
  renameSync(tmpPath, finalPath);
}

/**
 * Write a synthetic `done.json` for the failed-to-spawn case so the
 * boot reconciler doesn't see a "running" run with no tmux session
 * forever. Only `exitCode` lives in the file body (the wrapper's
 * normal contract); `endedAt` is derived from the file's mtime by
 * `readDoneFile`.
 */
function writeDoneSentinelBestEffort(
  runDir: string,
  exitCode: number,
): void {
  try {
    writeFileSync(join(runDir, "done.json"), JSON.stringify({ exitCode }));
  } catch (err) {
    log.warn("done sentinel write failed", {
      runDir,
      err: err instanceof Error ? err.message : String(err),
    });
  }
}

function materializeRun(
  meta: ActionMeta,
  runDir: string,
  lines: readonly ActionLine[],
): ActionRun {
  return {
    slug: meta.slug,
    kind: meta.kind,
    actionId: meta.actionId,
    actionName: meta.actionName,
    prompt: meta.prompt,
    startedAt: meta.startedAt,
    endedAt: meta.endedAt,
    status: meta.status,
    lines,
    runDir,
    affects: meta.affects,
  };
}

export const actionRegistry = new ActionRegistry();
