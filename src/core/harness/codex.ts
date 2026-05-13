/**
 * Codex harness impl. Codex stores one rollout jsonl per session under
 * `~/.codex/sessions/YYYY/MM/DD/rollout-<iso>-<uuid>.jsonl`, with the
 * first line a `session_meta` event carrying `payload.cwd` (the cwd
 * the user spawned codex from). We filter by exact cwd match against
 * the worktree path so each worktree shows its own session set.
 *
 * Resume: `codex resume <uuid>`. Fresh: `codex` (no args). Codex
 * generates the new session id itself; we never specify one.
 *
 * Tmux session naming: single slot per slug (`<slug>-codex`) for v1.
 * Switching codex sessions on the same worktree requires detaching
 * and respawning; multi-tmux-per-slug is a followup.
 */
import {
  closeSync,
  existsSync,
  openSync,
  readdirSync,
  readFileSync,
  readSync,
  statSync,
} from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

import { createLogger } from "../logger.ts";

import type { Harness, HarnessSession, HarnessSpawnArgs } from "./types.ts";

const log = createLogger("[codex]");

const CODEX_GLYPH = "\u{F4AC}"; // nf-oct-cloud
const CODEX_COLOR = "#4d56d6";
const CODEX_TMUX_INFIX = "-codex";

const CODEX_SESSIONS_DIR = join(homedir(), ".codex", "sessions");
const CODEX_SESSION_INDEX = join(homedir(), ".codex", "session_index.jsonl");
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

  tmuxSessionName(slug, _managedName) {
    // Single-tmux-per-slug for v1. The managedName is ignored — codex
    // sessions are tracked by their resume id, and the tmux slot is
    // a shared `<slug>-codex` regardless of which session is running
    // inside. Multi-tmux-per-slug for codex is a followup.
    return `${slug}${CODEX_TMUX_INFIX}`;
  },

  async discoverSessions({ slug, wtPath }) {
    const titles = readSessionIndex();
    const rollouts = scanRollouts(wtPath);
    // Single-tmux-per-slug for v1: every codex session reports the
    // bare `<slug>-codex` tmux name. `useHarnessSessions` re-annotates
    // `isLive` against the current tmux name set; here we set false.
    const tmuxName = `${slug}${CODEX_TMUX_INFIX}`;
    const out: HarnessSession[] = [];
    for (const r of rollouts) {
      const title = titles.get(r.sessionId) ?? r.sessionId.slice(0, 8);
      out.push({
        displayName: title,
        sessionId: r.sessionId,
        tmuxSessionName: tmuxName,
        lastActiveMs: r.mtimeMs,
        isLive: false,
        extras: { managedName: null, derivedState: null, queued: 0 },
      });
    }
    out.sort((a, b) => (b.lastActiveMs ?? 0) - (a.lastActiveMs ?? 0));
    return out;
  },

  buildArgs(args: HarnessSpawnArgs) {
    if (args.resumeSessionId !== null) {
      return ["codex", "resume", args.resumeSessionId];
    }
    return ["codex"];
  },

  reapState(_liveSlugs) {
    // No-op: codex owns its session files; we don't write any wt
    // state on its behalf, so nothing to reap.
  },
};

type RolloutMeta = {
  sessionId: string;
  cwd: string;
  mtimeMs: number;
};

/**
 * Walk the codex sessions tree newest-first, parse the `session_meta`
 * line out of each rollout, and return rollouts whose cwd matches the
 * given worktree path. Caps at `SCAN_MAX_DAYS` days to keep the scan
 * bounded; very old sessions are dropped from the picker.
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
          out.push(meta);
        }
      }
    }
  }
  return out;
}

/**
 * Read only the first line of a rollout, parse the `session_meta`
 * event, and return its `payload.id` + `payload.cwd`. The file is
 * synthesised by codex so the first line is reliably session_meta.
 * Returns null on any read/parse failure — corrupt or empty rollouts
 * just don't surface in the picker.
 */
function readRolloutMeta(path: string): RolloutMeta | null {
  let stat;
  try {
    stat = statSync(path);
  } catch {
    return null;
  }
  // Read only enough bytes to capture the first line. Session_meta
  // lines are big (full system prompt embedded) — 32 KB is plenty.
  let buf: Uint8Array;
  try {
    const fd = openSync(path, "r");
    try {
      buf = new Uint8Array(Math.min(stat.size, 64 * 1024));
      readSync(fd, buf, 0, buf.length, 0);
    } finally {
      closeSync(fd);
    }
  } catch {
    return null;
  }
  const text = new TextDecoder().decode(buf);
  const newlineIdx = text.indexOf("\n");
  const firstLine = newlineIdx >= 0 ? text.slice(0, newlineIdx) : text;
  try {
    const obj = JSON.parse(firstLine) as {
      type?: string;
      payload?: { id?: string; cwd?: string };
    };
    if (obj.type !== "session_meta") return null;
    const id = obj.payload?.id;
    const cwd = obj.payload?.cwd;
    if (typeof id !== "string" || typeof cwd !== "string") return null;
    return { sessionId: id, cwd, mtimeMs: stat.mtimeMs };
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

/** Recognise a codex tmux session for a slug. */
export function isCodexTmuxName(name: string, slug: string): boolean {
  return name === `${slug}${CODEX_TMUX_INFIX}`;
}
