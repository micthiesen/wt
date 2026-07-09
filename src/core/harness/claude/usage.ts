/**
 * Reader for the Claude Code statusline's usage cache. The statusline
 * script (`~/.claude/statusline.sh`) hits Anthropic's
 * `/api/oauth/usage` endpoint at most once every 5 minutes and writes
 * the response to a JSON file. We piggyback on that cache so the TUI
 * can show the same `5h X% / 7d Y%` rollup without making its own
 * authenticated HTTP call.
 */
import { readFileSync, statSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

export type UsagePeriod = {
  /** Percentage utilization of the window. */
  utilization: number;
  /** ISO8601 timestamp when this window resets, or null when unknown. */
  resetsAt: string | null;
};

export type ClaudeUsage = {
  fiveHour: UsagePeriod;
  sevenDay: UsagePeriod;
  /**
   * Cache-file mtime in epoch ms. Lets the consumer decide whether the
   * cache is too stale to render — the statusline considers anything
   * older than 30 minutes "TBD" and we mirror that policy in the UI.
   */
  cachedAtMs: number;
};

const CACHE_PATH = join(homedir(), ".cache", "claude-statusline-usage.json");

/**
 * Load and parse the cache. Returns null when the file is missing or
 * malformed. Cheap fs read + JSON parse; safe to call on every render.
 */
export function readClaudeUsage(): ClaudeUsage | null {
  let stat;
  try {
    stat = statSync(CACHE_PATH);
  } catch {
    return null;
  }
  let raw: string;
  try {
    raw = readFileSync(CACHE_PATH, "utf8");
  } catch {
    return null;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(raw);
  } catch {
    return null;
  }
  if (!parsed || typeof parsed !== "object") return null;
  const obj = parsed as Record<string, unknown>;
  const five = period(obj.five_hour);
  const seven = period(obj.seven_day);
  if (!five || !seven) return null;
  return { fiveHour: five, sevenDay: seven, cachedAtMs: stat.mtimeMs };
}

function period(raw: unknown): UsagePeriod | null {
  if (!raw || typeof raw !== "object") return null;
  const r = raw as Record<string, unknown>;
  if (typeof r.utilization !== "number") return null;
  return {
    utilization: r.utilization,
    resetsAt: typeof r.resets_at === "string" ? r.resets_at : null,
  };
}
