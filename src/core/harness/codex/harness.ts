/**
 * Codex harness impl. Codex stores one rollout jsonl per session under
 * `~/.codex/sessions/YYYY/MM/DD/rollout-<iso>-<uuid>.jsonl`, with the
 * first line a `session_meta` event carrying `payload.cwd` (the cwd
 * the user spawned codex from). We filter by exact cwd match against
 * the worktree path and interactive-user provenance so internal
 * subagents / `codex exec` runs don't masquerade as resumable sessions.
 *
 * Resume: `codex resume <uuid>`. Fresh: `codex` (no args). Codex
 * generates the new session id itself; we never specify one.
 *
 * Tmux session naming: single slot per slug (`<slug>-codex`) for v1.
 * Switching codex sessions on the same worktree requires detaching
 * and respawning; multi-tmux-per-slug is a followup.
 */
import { Effect } from "effect";
import {
  existsSync,
  readdirSync,
  statSync,
} from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

import { isRiftWorktree } from "../../backend.ts";
import { createLogger } from "../../logger.ts";
import { readFileSlice } from "../../tail-util.ts";
import type { DerivedState } from "../status.ts";
import {
  reapCodexNames,
  reconcileCodexNames,
} from "./names.ts";
import { trustCodexWorkspace } from "./trust.ts";
import { discoverCodexSessionsInWorker } from "./discovery.ts";
import { readCodexNativeSnapshots } from "./app-server.ts";
import { enrichCodexSessionsWithNativeStatus } from "./native-status.ts";
import { CODEX_MAIN_PROMPT, CODEX_MANAGER_PROMPT, codexRolloutBelongsToSlot } from "./slot.ts";

import type { Harness, HarnessSession, HarnessSpawnArgs } from "../types.ts";

const log = createLogger("[codex]");

const CODEX_GLYPH = "\u{F4AC}"; // nf-oct-cloud
const CODEX_COLOR = "#4d56d6";
const CODEX_TMUX_INFIX = "-codex";
const CODEX_SESSIONS_DIR = join(homedir(), ".codex", "sessions");
/** Initial backwards window for state derivation. Expanded when a large
 * response/tool line pushed the latest task lifecycle marker farther back. */
const TAIL_BYTES = 64 * 1024;
const MAX_TAIL_SCAN_BYTES = 8 * 1024 * 1024;
/**
 * How far back to walk the date-partitioned sessions tree. Codex names
 * directories `YYYY/MM/DD` so we list the year dirs, then month, then
 * day, descending by name. 30 days is a generous cap — sessions older
 * than that don't usefully resurface in the picker.
 */
const SCAN_MAX_DAYS = 30;

export const codexHarness: Harness = {
  id: "codex",
  label: "Codex",
  letter: "x",
  glyph: CODEX_GLYPH,
  color: CODEX_COLOR,
  singleSlot: true,
  // Codex skills are invoked with a `$` prefix (e.g. $restack).
  skillPrefix: "$",
  // Codex receives a bracketed paste as a multi-line input blob; the
  // first Enter only exits that state, so a second is needed to
  // actually submit the prompt.
  injectSubmitKeys: ["Enter", "Enter"],

  tmuxSessionName(slug, _managedName) {
    // Single-tmux-per-slug for v1. The managedName is ignored — codex
    // sessions are tracked by their resume id, and the tmux slot is
    // a shared `<slug>-codex` regardless of which session is running
    // inside. Multi-tmux-per-slug for codex is a followup.
    return `${slug}${CODEX_TMUX_INFIX}`;
  },

  async discoverSessions({ slug, wtPath, signal, liveSessionId }) {
    const sessions = await discoverCodexSessionsInWorker(
      slug,
      wtPath,
      signal,
      liveSessionId,
    );
    if (sessions.length === 0) return sessions;
    try {
      const snapshots = await Effect.runPromise(
        readCodexNativeSnapshots(sessions.map((session) => session.sessionId)),
        signal ? { signal } : undefined,
      );
      return enrichCodexSessionsWithNativeStatus(sessions, snapshots);
    } catch (cause) {
      // TanStack cancellation must remain cancellation. Swallowing the abort
      // as an optional-daemon failure lets superseded discovery populate the
      // cache after its observer has moved on.
      if (signal?.aborted) throw cause;
      // The daemon is optional and user-managed. Rollout state remains the
      // honest fallback when its local socket is absent or incompatible.
      return sessions;
    }
  },

  buildArgs(args: HarnessSpawnArgs) {
    if (args.resumeSessionId !== null) {
      return ["codex", "resume", args.resumeSessionId];
    }
    if (args.slug === "manager") return ["codex", CODEX_MANAGER_PROMPT];
    if (args.slug === "main") return ["codex", CODEX_MAIN_PROMPT];
    return ["codex"];
  },

  ensureTrusted(wtPath) {
    // Same rationale as Claude's: only a rift checkout (an independent clone)
    // trips Codex's per-project trust gate; a git worktree inherits the main
    // repo's trust.
    return Effect.sync(() => {
      if (isRiftWorktree(wtPath)) trustCodexWorkspace(wtPath);
    });
  },

  reapState(liveSlugs) {
    reapCodexNames(liveSlugs);
  },
};

/**
 * Synchronous discovery implementation used only by discovery-worker.ts.
 * Keeping it beside the existing rollout/tail parsers avoids a second copy of
 * Codex's filtering and state-derivation rules without putting sync I/O back
 * on the TUI thread.
 */
export function discoverCodexSessionsSync(
  slug: string,
  wtPath: string,
  sessionsDir = CODEX_SESSIONS_DIR,
  liveSessionId: string | null = null,
): HarnessSession[] {
  const rollouts = scanRollouts(wtPath, slug, sessionsDir);
  if (liveSessionId && !rollouts.some((rollout) => rollout.sessionId === liveSessionId)) {
    const exact = findCodexRolloutForSession(
      wtPath,
      slug,
      liveSessionId,
      sessionsDir,
    );
    if (exact) {
      rollouts.push({ sessionId: liveSessionId, cwd: wtPath, ...exact });
    }
  }
  rollouts.sort((a, b) => b.mtimeMs - a.mtimeMs);
  const friendlyNames = reconcileCodexNames(
    slug,
    rollouts.map((r) => r.sessionId),
  );
  // Single-tmux-per-slug for v1: every codex session reports the
  // bare `<slug>-codex` tmux name. `useHarnessSessions` re-annotates
  // `isLive` against the current tmux name set; here we set false.
  const tmuxName = `${slug}${CODEX_TMUX_INFIX}`;
  // Track the most-recent rollout path per session so the event poller
  // can find the right file without its own scan.
  const out: HarnessSession[] = [];
  for (const r of rollouts) {
    // The wt name is the stable UI and resume identity. Codex's generated
    // thread title is intentionally not used here: it changes independently
    // and previously hid which UUID wt had assigned to `primary`.
    const managedName = friendlyNames[r.sessionId] ?? r.sessionId.slice(0, 8);
    const tail = readCodexTail(r.path, r.mtimeMs, r.size);
    out.push({
      displayName: managedName,
      sessionId: r.sessionId,
      tmuxSessionName: tmuxName,
      lastActiveMs: r.mtimeMs,
      isLive: false,
      extras: {
        managedName,
        // Liveness-independent best guess; `useHarnessSessions`
        // finalizes it against the live tmux set (dead cleanly → idle,
        // dead mid-turn → abandoned, live slot keeps working/waiting).
        derivedState: tail ? deriveCodexState(tail) : null,
        waitingFor: tail?.pendingInteraction === "approval"
          ? "approval prompt"
          : tail?.pendingInteraction === "question"
            ? "question prompt"
            : null,
        queued: 0,
        // Stash last-event time for displays that care about message age.
        tailEndedAt: tail?.lastEventMs ?? null,
      },
    });
  }
  return out;
}

type RolloutMeta = {
  sessionId: string;
  cwd: string;
  path: string;
  mtimeMs: number;
  size: number;
};

export type CodexRolloutFile = Pick<RolloutMeta, "path" | "mtimeMs" | "size">;

/**
 * Resolve one mapped Codex thread to its rollout. Unlike picker discovery,
 * this deliberately walks every date partition: a resumed thread keeps
 * appending to the rollout in its original creation-day directory.
 *
 * Identity comes from session_meta rather than the filename, and the normal
 * main/manager ownership filter still applies before a rollout can be used.
 */
export function findCodexRolloutForSession(
  cwd: string,
  slug: string,
  sessionId: string,
  sessionsDir = CODEX_SESSIONS_DIR,
): CodexRolloutFile | null {
  if (!existsSync(sessionsDir)) return null;
  const cacheKey = `${cwd}\0${slug}\0${sessionId}\0${sessionsDir}`;
  const cachedPath = exactRolloutPathCache.get(cacheKey);
  if (cachedPath) {
    const meta = readRolloutMeta(cachedPath);
    if (
      meta?.sessionId === sessionId &&
      meta.cwd === cwd &&
      codexRolloutBelongsToSlot(cachedPath, meta.size, slug)
    ) {
      return { path: cachedPath, mtimeMs: meta.mtimeMs, size: meta.size };
    }
    exactRolloutPathCache.delete(cacheKey);
  }
  let best: CodexRolloutFile | null = null;
  let years: string[];
  try { years = readdirSync(sessionsDir); } catch { return null; }
  for (const year of years) {
    const yearPath = join(sessionsDir, year);
    let months: string[];
    try { months = readdirSync(yearPath); } catch { continue; }
    for (const month of months) {
      const monthPath = join(yearPath, month);
      let days: string[];
      try { days = readdirSync(monthPath); } catch { continue; }
      for (const day of days) {
        const dayPath = join(monthPath, day);
        let files: string[];
        try { files = readdirSync(dayPath); } catch { continue; }
        for (const file of files) {
          if (!file.startsWith("rollout-") || !file.endsWith(".jsonl")) continue;
          // UUID is part of Codex's rollout filename. Avoid opening every
          // historical transcript on each legacy-readiness poll.
          if (!file.includes(sessionId)) continue;
          const path = join(dayPath, file);
          const meta = readRolloutMeta(path);
          if (!meta || meta.sessionId !== sessionId || meta.cwd !== cwd) continue;
          if (!codexRolloutBelongsToSlot(path, meta.size, slug)) continue;
          if (!best || meta.mtimeMs > best.mtimeMs) {
            best = { path, mtimeMs: meta.mtimeMs, size: meta.size };
          }
        }
      }
    }
  }
  if (best) {
    if (exactRolloutPathCache.size >= 512) exactRolloutPathCache.clear();
    exactRolloutPathCache.set(cacheKey, best.path);
  }
  return best;
}

/** Exact live UUID lookups cross every date partition once, then stat this path. */
const exactRolloutPathCache = new Map<string, string>();

/**
 * Return the most-recently-modified rollout path for the given cwd, or
 * null when none exist. Stops after the first matching file found when
 * walking newest-first (significantly cheaper than full scanRollouts for
 * the polling hot path). Caps at SCAN_MAX_DAYS to bound the walk.
 */
export function latestRolloutForCwd(cwd: string, slug: string, sessionsDir = CODEX_SESSIONS_DIR): { path: string; mtimeMs: number; size: number } | null {
  if (!existsSync(sessionsDir)) return null;
  let daysScanned = 0;
  let years: string[];
  try {
    years = readdirSync(sessionsDir).sort().reverse();
  } catch {
    return null;
  }
  let best: { path: string; mtimeMs: number; size: number } | null = null;
  for (const y of years) {
    const yPath = join(sessionsDir, y);
    let months: string[];
    try { months = readdirSync(yPath).sort().reverse(); } catch { continue; }
    for (const m of months) {
      const mPath = join(yPath, m);
      let days: string[];
      try { days = readdirSync(mPath).sort().reverse(); } catch { continue; }
      for (const d of days) {
        if (daysScanned >= SCAN_MAX_DAYS) return best;
        daysScanned++;
        const dPath = join(mPath, d);
        let files: string[];
        try { files = readdirSync(dPath).sort().reverse(); } catch { continue; }
        for (const f of files) {
          if (!f.startsWith("rollout-") || !f.endsWith(".jsonl")) continue;
          const filePath = join(dPath, f);
          const meta = readRolloutMeta(filePath);
          if (!meta || meta.cwd !== cwd) continue;
          if (!codexRolloutBelongsToSlot(filePath, meta.size, slug)) continue;
          if (!best || meta.mtimeMs > best.mtimeMs) {
            best = { path: filePath, mtimeMs: meta.mtimeMs, size: meta.size };
          }
        }
      }
    }
  }
  return best;
}

/**
 * Walk the codex sessions tree newest-first, parse the `session_meta`
 * line out of each rollout, and return interactive user sessions whose
 * cwd matches the given worktree path. Caps at `SCAN_MAX_DAYS` days to
 * keep the scan bounded; very old sessions are dropped from the picker.
 */
function scanRollouts(wtPath: string, slug: string, sessionsDir: string): RolloutMeta[] {
  if (!existsSync(sessionsDir)) return [];
  const out: RolloutMeta[] = [];
  let daysScanned = 0;
  let years: string[];
  try {
    years = readdirSync(sessionsDir).sort().reverse();
  } catch (err) {
    log.warn("readdir failed", { err: String(err) });
    return [];
  }
  for (const y of years) {
    const yPath = join(sessionsDir, y);
    let months: string[];
    try {
      months = readdirSync(yPath).sort().reverse();
    } catch {
      continue;
    }
    for (const m of months) {
      const mPath = join(yPath, m);
      let days: string[];
      try {
        days = readdirSync(mPath).sort().reverse();
      } catch {
        continue;
      }
      for (const d of days) {
        if (daysScanned >= SCAN_MAX_DAYS) return out;
        daysScanned++;
        const dPath = join(mPath, d);
        let files: string[];
        try {
          files = readdirSync(dPath);
        } catch {
          continue;
        }
        for (const f of files) {
          if (!f.startsWith("rollout-") || !f.endsWith(".jsonl")) continue;
          const filePath = join(dPath, f);
          const meta = readRolloutMeta(filePath);
          if (!meta) continue;
          if (meta.cwd !== wtPath) continue;
          if (!codexRolloutBelongsToSlot(filePath, meta.size, slug)) continue;
          out.push({ ...meta, path: filePath });
        }
      }
    }
  }
  return out;
}

/** Intermediate return from readRolloutMeta (path is added by scanner). */
type RolloutMetaRaw = Omit<RolloutMeta, "path">;

/**
 * A rollout's first line (session_meta) is written once at creation and
 * never changes, and rollout filenames embed a uuid, so the parsed
 * identity is cacheable by path for the process lifetime. This matters:
 * the 3s `harnessSessionsQuery` poll and the 2.5s tail/event pollers
 * each walk the sessions tree and would otherwise re-open + re-parse a
 * 64KB head per rollout per tick. Only SUCCESSFUL parses are cached — a
 * just-created rollout can be read before codex flushes the first line,
 * and a cached failure would hide that session forever.
 */
const rolloutIdentityCache = new Map<string, { sessionId: string; cwd: string }>();
const ROLLOUT_IDENTITY_CACHE_MAX = 8192;

/**
 * Read only the first line of a rollout, parse the `session_meta`
 * event, and return its `payload.id` + `payload.cwd` when it represents
 * an interactive user thread. The same cwd also appears on Codex's
 * internal guardian/subagent rollouts and `codex exec` runs; those are
 * not resumable F12 conversations and must not enter discovery.
 * Returns null on any read/parse failure or non-interactive rollout.
 */
function readRolloutMeta(path: string): RolloutMetaRaw | null {
  let stat;
  try {
    stat = statSync(path);
  } catch {
    return null;
  }
  const cached = rolloutIdentityCache.get(path);
  if (cached) {
    return { ...cached, mtimeMs: stat.mtimeMs, size: stat.size };
  }
  // Read only enough bytes to capture the first line. Session_meta
  // lines are big (full system prompt embedded) — 32 KB is plenty.
  let text: string;
  try {
    text = readFileSlice(path, 0, Math.min(stat.size, 64 * 1024));
  } catch {
    return null;
  }
  const newlineIdx = text.indexOf("\n");
  const firstLine = newlineIdx >= 0 ? text.slice(0, newlineIdx) : text;
  try {
    const obj = JSON.parse(firstLine) as {
      type?: string;
      payload?: {
        id?: string;
        cwd?: string;
        originator?: string;
        thread_source?: string;
      };
    };
    if (obj.type !== "session_meta") return null;
    const id = obj.payload?.id;
    const cwd = obj.payload?.cwd;
    if (typeof id !== "string" || typeof cwd !== "string") return null;
    // Match `codex resume`'s default interactive-session scope. Codex
    // 0.153 started preserving wt's launch originator (`wt`) on the root
    // conversation; older versions wrote `codex-tui`. Guardian/subagent
    // rollouts can use either originator, so `thread_source: user` remains
    // the discriminator that keeps them out of the picker.
    if (
      (obj.payload?.originator !== "codex-tui" && obj.payload?.originator !== "wt") ||
      obj.payload?.thread_source !== "user"
    ) {
      return null;
    }
    // Runaway backstop only — the 30-day window holds far fewer entries.
    if (rolloutIdentityCache.size >= ROLLOUT_IDENTITY_CACHE_MAX) {
      rolloutIdentityCache.clear();
    }
    rolloutIdentityCache.set(path, { sessionId: id, cwd });
    return { sessionId: id, cwd, mtimeMs: stat.mtimeMs, size: stat.size };
  } catch {
    return null;
  }
}

// ---------------------------------------------------------------------------
// Tail reading + state derivation
// ---------------------------------------------------------------------------

/**
 * Parsed result of reading a rollout tail. Exported so the event poller
 * can reuse the same cache keyed on `(path, mtimeMs, size)`.
 */
export type CodexTailResult = {
  /** True when the last turn in the tail ended cleanly (task_complete or
   *  turn_aborted). False means an unmatched task_started was found. */
  tailClosedCleanly: boolean;
  /** The latest lifecycle marker, or null when no positive lifecycle
   * evidence was readable. Terminal injection treats null as unsafe. */
  lastTaskEventKind: ParsedCodexTaskEvent["kind"] | null;
  /** A native interaction request observed in the active turn. */
  pendingInteraction: "question" | "approval" | null;
  /** False when the sampled tail contained an incomplete/malformed JSON line. */
  tailParseComplete: boolean;
  /** Mtime of the file at read time, for freshness comparisons. */
  lastEventMs: number;
};

type TailCacheEntry = {
  mtimeMs: number;
  size: number;
  result: CodexTailResult;
};
const tailCache = new Map<string, TailCacheEntry>();
const TAIL_CACHE_MAX = 128;

/**
 * Read the rollout tail and derive whether the most recent turn ended
 * cleanly. Cached on (path, mtimeMs, size). Starts with a small tail read
 * and expands backwards only when no task lifecycle marker is found; Codex
 * writes very large `response_item` lines while working, and those can push
 * `task_started` outside a fixed 64KB tail before `task_complete` appears.
 *
 * Returns null when the file is empty, unreadable, or has no
 * task_started events at all (very short sessions).
 */
export function readCodexTail(
  path: string,
  mtimeMs: number,
  size: number,
): CodexTailResult | null {
  if (size === 0) return null;

  const cached = tailCache.get(path);
  if (cached && cached.mtimeMs === mtimeMs && cached.size === size) {
    return cached.result;
  }

  let parsed: ParsedCodexTail = { latest: null, pendingInteraction: null, malformed: false };
  let windowBytes = Math.min(TAIL_BYTES, size);
  while (true) {
    try {
      parsed = parseCodexTailWindow(path, size, windowBytes);
    } catch {
      return null;
    }
    if (parsed.latest !== null || windowBytes >= size || windowBytes >= MAX_TAIL_SCAN_BYTES) {
      break;
    }
    windowBytes = Math.min(size, windowBytes * 4, MAX_TAIL_SCAN_BYTES);
  }

  const tailClosedCleanly =
    parsed.latest === null ||
    parsed.latest.kind === "task_complete" ||
    parsed.latest.kind === "turn_aborted";
  const result: CodexTailResult = {
    tailClosedCleanly,
    lastTaskEventKind: parsed.latest?.kind ?? null,
    pendingInteraction: parsed.pendingInteraction,
    tailParseComplete: !parsed.malformed,
    lastEventMs: parsed.latest?.ts ?? mtimeMs,
  };
  setCached(path, mtimeMs, size, result);
  return result;
}

type ParsedCodexTaskEvent = {
  kind: "task_started" | "task_complete" | "turn_aborted";
  ts: number | null;
};

type ParsedCodexTail = {
  latest: ParsedCodexTaskEvent | null;
  pendingInteraction: "question" | "approval" | null;
  malformed: boolean;
};

function parseCodexTailWindow(
  path: string,
  size: number,
  windowBytes: number,
): ParsedCodexTail {
  const start = Math.max(0, size - windowBytes);
  const text = readFileSlice(path, start, size - start);
  // If we didn't start at byte 0, the first line is likely partial.
  const lines = text.split("\n");
  const startIdx = start > 0 ? 1 : 0;
  let latest: ParsedCodexTaskEvent | null = null;
  let pendingInteraction: ParsedCodexTail["pendingInteraction"] = null;
  let malformed = false;
  for (let i = startIdx; i < lines.length; i++) {
    const line = lines[i];
    if (!line) continue;
    let obj: Record<string, unknown>;
    try {
      obj = JSON.parse(line) as Record<string, unknown>;
    } catch {
      malformed = true;
      continue;
    }
    const payload = obj.payload;
    if (typeof payload !== "object" || payload === null) continue;
    const p = payload as Record<string, unknown>;
    if (
      obj.type === "response_item" &&
      (p.type === "function_call" || p.type === "custom_tool_call") &&
      p.name === "request_user_input"
    ) {
      pendingInteraction = "question";
      continue;
    }
    if (
      obj.type === "response_item" &&
      pendingInteraction !== null &&
      (p.type === "function_call_output" || p.type === "custom_tool_call_output")
    ) {
      // The answer/approval was delivered and the active turn can continue.
      pendingInteraction = null;
      continue;
    }
    if (
      obj.type === "event_msg" &&
      typeof p.type === "string" &&
      p.type.includes("approval") &&
      p.type.includes("request")
    ) {
      pendingInteraction = "approval";
      continue;
    }
    if (
      obj.type === "event_msg" &&
      typeof p.type === "string" &&
      p.type.includes("approval") &&
      (p.type.includes("response") || p.type.includes("resolved"))
    ) {
      pendingInteraction = null;
      continue;
    }
    if (obj.type !== "event_msg") continue;
    const ptype = p.type;
    if (
      ptype !== "task_started" &&
      ptype !== "task_complete" &&
      ptype !== "turn_aborted"
    ) {
      continue;
    }
    const ts = obj.timestamp;
    latest = {
      kind: ptype,
      ts: typeof ts === "string" ? Date.parse(ts) : null,
    };
    // A new or closed lifecycle marker supersedes interaction requests from
    // the previous turn. Requests following task_started set this again.
    pendingInteraction = null;
  }
  return { latest, pendingInteraction, malformed };
}

function setCached(
  path: string,
  mtimeMs: number,
  size: number,
  result: CodexTailResult,
): void {
  if (tailCache.size >= TAIL_CACHE_MAX) {
    const first = tailCache.keys().next().value;
    if (first !== undefined) tailCache.delete(first);
  }
  tailCache.set(path, { mtimeMs, size, result });
}

/**
 * Liveness-independent best guess for a codex session's state from its
 * rollout tail: an unmatched `task_started` (mid-turn) reads as `working`,
 * a cleanly-closed turn as `waiting`. This mirrors opencode's
 * `deriveOpencodeState` — `computeHarnessSessions` finalizes it against
 * real tmux liveness, demoting a cleanly closed dead slot to `idle` and
 * a mid-turn dead slot to `abandoned` while a live slot keeps this guess
 * (so a working codex reads `working`, not the floor-`waiting` the old
 * isLive-baked path produced).
 */
export function deriveCodexState(tail: CodexTailResult): DerivedState {
  if (tail.pendingInteraction !== null) return "asking";
  return tail.tailClosedCleanly ? "waiting" : "working";
}

/** Recognise a codex tmux session for a slug. */
export function isCodexTmuxName(name: string, slug: string): boolean {
  return name === `${slug}${CODEX_TMUX_INFIX}`;
}
