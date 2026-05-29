/**
 * Reader for Codex's rate-limit usage. Codex writes a `token_count`
 * event into its rollout jsonl on each turn, carrying the same two
 * windows the Claude statusline exposes: `primary` (5h, 300 min) and
 * `secondary` (7d, 10080 min), each with a `used_percent` and a
 * `resets_at` epoch-seconds stamp. We read the most-recently-modified
 * rollout's latest such event — no HTTP call, no separate cache file.
 *
 * Account-global: any recent rollout reflects current limits, so the
 * newest one across the whole sessions tree is the freshest source
 * regardless of which worktree it belongs to.
 */
import { closeSync, openSync, readdirSync, readSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

import type { UsagePeriod } from "../claude-usage.ts";
import { createLogger } from "../logger.ts";

const log = createLogger("[codex-usage]");

const SESSIONS_DIR = join(homedir(), ".codex", "sessions");
/** Trailing bytes to scan for the latest `token_count` event. */
const TAIL_BYTES = 96 * 1024;
/** Day-dirs to consider when finding the newest rollout (covers a
 *  session running across a midnight boundary). */
const SCAN_DAY_DIRS = 3;

export type CodexUsage = {
  fiveHour: UsagePeriod;
  sevenDay: UsagePeriod;
  /** Plan tier from the rate-limit payload (e.g. "plus"), or null. */
  planType: string | null;
  /** Newest rollout mtime, epoch ms — drives the staleness gate. */
  cachedAtMs: number;
};

/**
 * Walk the date-partitioned sessions tree newest-first and return the
 * rollout with the greatest mtime across the most recent `SCAN_DAY_DIRS`
 * day directories. Bounded so the scan stays cheap on every refetch.
 */
function findLatestRollout(): { path: string; mtimeMs: number; size: number } | null {
  let best: { path: string; mtimeMs: number; size: number } | null = null;
  let daysSeen = 0;
  let years: string[];
  try {
    years = readdirSync(SESSIONS_DIR).sort().reverse();
  } catch {
    return null;
  }
  for (const y of years) {
    let months: string[];
    try {
      months = readdirSync(join(SESSIONS_DIR, y)).sort().reverse();
    } catch {
      continue;
    }
    for (const m of months) {
      let days: string[];
      try {
        days = readdirSync(join(SESSIONS_DIR, y, m)).sort().reverse();
      } catch {
        continue;
      }
      for (const d of days) {
        if (daysSeen >= SCAN_DAY_DIRS) return best;
        daysSeen++;
        const dir = join(SESSIONS_DIR, y, m, d);
        let files: string[];
        try {
          files = readdirSync(dir);
        } catch {
          continue;
        }
        for (const f of files) {
          if (!f.startsWith("rollout-") || !f.endsWith(".jsonl")) continue;
          const path = join(dir, f);
          try {
            const st = statSync(path);
            if (!best || st.mtimeMs > best.mtimeMs) {
              best = { path, mtimeMs: st.mtimeMs, size: st.size };
            }
          } catch {
            // skip unreadable
          }
        }
      }
    }
  }
  return best;
}

function period(raw: unknown): UsagePeriod | null {
  if (!raw || typeof raw !== "object") return null;
  const r = raw as Record<string, unknown>;
  if (typeof r.used_percent !== "number") return null;
  // `resets_at` is epoch SECONDS here (Claude's is an ISO string); the
  // shared `UsagePeriod.resetsAt` is ISO, so normalize.
  const resetsAt =
    typeof r.resets_at === "number"
      ? new Date(r.resets_at * 1000).toISOString()
      : null;
  return { utilization: r.used_percent, resetsAt };
}

/**
 * Read the newest rollout's last `token_count.rate_limits`. Returns null
 * when no rollout exists or none carries a rate-limit block (e.g. a
 * fresh session before its first turn).
 */
export function readCodexUsage(): CodexUsage | null {
  const latest = findLatestRollout();
  if (!latest || latest.size === 0) return null;

  const start = Math.max(0, latest.size - TAIL_BYTES);
  let text: string;
  try {
    const fd = openSync(latest.path, "r");
    try {
      const len = latest.size - start;
      const buf = Buffer.alloc(len);
      readSync(fd, buf, 0, len, start);
      text = buf.toString("utf8");
    } finally {
      closeSync(fd);
    }
  } catch (err) {
    log.debug("rollout tail read failed", { err: String(err) });
    return null;
  }

  const lines = text.split("\n");
  // Scan backward for the most recent token_count carrying rate_limits.
  for (let i = lines.length - 1; i >= 0; i--) {
    const line = lines[i];
    if (!line || !line.includes("rate_limits")) continue;
    let obj: Record<string, unknown>;
    try {
      obj = JSON.parse(line) as Record<string, unknown>;
    } catch {
      continue;
    }
    if (obj.type !== "event_msg") continue;
    const payload = obj.payload as Record<string, unknown> | undefined;
    if (!payload || payload.type !== "token_count") continue;
    const rl = payload.rate_limits as Record<string, unknown> | undefined;
    if (!rl) continue;
    const five = period(rl.primary);
    const seven = period(rl.secondary);
    if (!five || !seven) continue;
    return {
      fiveHour: five,
      sevenDay: seven,
      planType: typeof rl.plan_type === "string" ? rl.plan_type : null,
      cachedAtMs: latest.mtimeMs,
    };
  }
  return null;
}
