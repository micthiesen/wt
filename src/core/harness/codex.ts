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
import {
  existsSync,
  readdirSync,
  readFileSync,
  statSync,
} from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

import { createLogger } from "../logger.ts";
import { readFileSlice } from "../tail-util.ts";
import type { DerivedState } from "../claude-status.ts";
import {
  reapCodexNames,
  reconcileCodexNames,
} from "../codex-sessions.ts";

import type { Harness, HarnessSession, HarnessSpawnArgs } from "./types.ts";

const log = createLogger("[codex]");

const CODEX_GLYPH = "\u{F4AC}"; // nf-oct-cloud
const CODEX_COLOR = "#4d56d6";
const CODEX_TMUX_INFIX = "-codex";

const CODEX_SESSIONS_DIR = join(homedir(), ".codex", "sessions");
const CODEX_SESSION_INDEX = join(homedir(), ".codex", "session_index.jsonl");
/** How far back from end-of-file to read for state derivation. */
const TAIL_BYTES = 64 * 1024;
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

  async discoverSessions({ slug, wtPath }) {
    const titles = readSessionIndex();
    const rollouts = scanRollouts(wtPath).sort((a, b) => b.mtimeMs - a.mtimeMs);
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
      // A Codex-native `/rename` title wins when present. Otherwise use
      // wt's stable friendly name; UUID prefixes are only a defensive
      // fallback if persistence fails.
      const title =
        titles.get(r.sessionId) ??
        friendlyNames[r.sessionId] ??
        r.sessionId.slice(0, 8);
      const tail = readCodexTail(r.path, r.mtimeMs, r.size);
      out.push({
        displayName: title,
        sessionId: r.sessionId,
        tmuxSessionName: tmuxName,
        lastActiveMs: r.mtimeMs,
        isLive: false,
        extras: {
          managedName: null,
          // Liveness-independent best guess; `useHarnessSessions`
          // finalizes it against the live tmux set (dead slot → abandoned/
          // idle by age, live slot keeps working/waiting).
          derivedState: tail ? deriveCodexState(tail) : null,
          queued: 0,
          // Stash last-event time so the re-annotator can compute
          // abandoned vs idle when tmux flips dead.
          tailEndedAt: tail?.lastEventMs ?? null,
        },
      });
    }
    return out;
  },

  buildArgs(args: HarnessSpawnArgs) {
    if (args.resumeSessionId !== null) {
      return ["codex", "resume", args.resumeSessionId];
    }
    return ["codex"];
  },

  reapState(liveSlugs) {
    reapCodexNames(liveSlugs);
  },
};

type RolloutMeta = {
  sessionId: string;
  cwd: string;
  path: string;
  mtimeMs: number;
  size: number;
};

/**
 * Return the most-recently-modified rollout path for the given cwd, or
 * null when none exist. Stops after the first matching file found when
 * walking newest-first (significantly cheaper than full scanRollouts for
 * the polling hot path). Caps at SCAN_MAX_DAYS to bound the walk.
 */
export function latestRolloutForCwd(cwd: string): { path: string; mtimeMs: number; size: number } | null {
  if (!existsSync(CODEX_SESSIONS_DIR)) return null;
  let daysScanned = 0;
  let years: string[];
  try {
    years = readdirSync(CODEX_SESSIONS_DIR).sort().reverse();
  } catch {
    return null;
  }
  let best: { path: string; mtimeMs: number; size: number } | null = null;
  for (const y of years) {
    const yPath = join(CODEX_SESSIONS_DIR, y);
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
function scanRollouts(wtPath: string): RolloutMeta[] {
  if (!existsSync(CODEX_SESSIONS_DIR)) return [];
  const out: RolloutMeta[] = [];
  let daysScanned = 0;
  let years: string[];
  try {
    years = readdirSync(CODEX_SESSIONS_DIR).sort().reverse();
  } catch (err) {
    log.warn("readdir failed", { err: String(err) });
    return [];
  }
  for (const y of years) {
    const yPath = join(CODEX_SESSIONS_DIR, y);
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
    // writes guardian/subagent rollouts with originator `codex-tui`,
    // so checking cwd or originator alone is insufficient.
    if (
      obj.payload?.originator !== "codex-tui" ||
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

/**
 * Map of session id → user-given thread name from
 * `~/.codex/session_index.jsonl`. Used to label entries in the picker
 * with the same title the user sees inside codex itself.
 */
function readSessionIndex(): Map<string, string> {
  const out = new Map<string, string>();
  if (!existsSync(CODEX_SESSION_INDEX)) return out;
  let raw: string;
  try {
    raw = readFileSync(CODEX_SESSION_INDEX, "utf8");
  } catch (err) {
    log.warn("read session_index.jsonl failed", { err: String(err) });
    return out;
  }
  for (const line of raw.split("\n")) {
    if (!line.trim()) continue;
    try {
      const obj = JSON.parse(line) as { id?: string; thread_name?: string };
      if (typeof obj.id === "string" && typeof obj.thread_name === "string") {
        out.set(obj.id, obj.thread_name);
      }
    } catch {
      // skip malformed lines
    }
  }
  return out;
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
 * Read the last TAIL_BYTES of a rollout and derive whether the most
 * recent turn ended cleanly. Cached on (path, mtimeMs, size).
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

  let text: string;
  try {
    const start = Math.max(0, size - TAIL_BYTES);
    text = readFileSlice(path, start, size - start);
  } catch {
    return null;
  }

  // If we didn't start at byte 0, the first line is likely partial — drop it.
  const lines = text.split("\n");
  const startIdx = size > TAIL_BYTES ? 1 : 0;

  let lastStartedTurnId: string | null = null;
  let lastStartedTs: number | null = null;
  let lastClosedTurnId: string | null = null;

  for (let i = startIdx; i < lines.length; i++) {
    const line = lines[i];
    if (!line) continue;
    let obj: Record<string, unknown>;
    try {
      obj = JSON.parse(line) as Record<string, unknown>;
    } catch {
      continue;
    }
    if (obj.type !== "event_msg") continue;
    const payload = obj.payload;
    if (typeof payload !== "object" || payload === null) continue;
    const p = payload as Record<string, unknown>;
    const ptype = p.type;
    if (ptype === "task_started") {
      const turnId = p.turn_id;
      if (typeof turnId === "string") {
        lastStartedTurnId = turnId;
        const ts = obj.timestamp;
        lastStartedTs = typeof ts === "string" ? Date.parse(ts) : null;
      }
    } else if (ptype === "task_complete" || ptype === "turn_aborted") {
      const turnId = p.turn_id;
      if (typeof turnId === "string") {
        lastClosedTurnId = turnId;
      }
    }
  }

  if (lastStartedTurnId === null) {
    // No task_started in tail at all — treat as cleanly closed.
    const result: CodexTailResult = {
      tailClosedCleanly: true,
      lastEventMs: lastStartedTs ?? mtimeMs,
    };
    setCached(path, mtimeMs, size, result);
    return result;
  }

  const tailClosedCleanly = lastClosedTurnId === lastStartedTurnId;
  const result: CodexTailResult = {
    tailClosedCleanly,
    lastEventMs: lastStartedTs ?? mtimeMs,
  };
  setCached(path, mtimeMs, size, result);
  return result;
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
 * real tmux liveness, demoting a dead slot to `abandoned`/`idle` by age
 * while a live slot keeps this guess (so a working codex reads `working`,
 * not the floor-`waiting` the old isLive-baked path produced).
 */
export function deriveCodexState(tail: CodexTailResult): DerivedState {
  return tail.tailClosedCleanly ? "waiting" : "working";
}

/** Recognise a codex tmux session for a slug. */
export function isCodexTmuxName(name: string, slug: string): boolean {
  return name === `${slug}${CODEX_TMUX_INFIX}`;
}
