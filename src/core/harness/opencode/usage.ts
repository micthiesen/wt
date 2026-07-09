/**
 * Reader for OpenCode spend. OpenCode bills per token (no subscription
 * rate-limit window like Claude/Codex), but it records a `cost` on every
 * assistant message in its SQLite DB. We sum that cost over the same two
 * windows the other harnesses report utilization for — 5h and 7d — so
 * the top-bar slot stays parallel: `5h $X / 7d $Y` instead of percentages.
 *
 * Reuses the read-only DB handle from `opencode.ts`.
 */
import type { Statement } from "bun:sqlite";

import { createLogger } from "../../logger.ts";

import { openDb } from "./harness.ts";

const log = createLogger("[opencode-usage]");

const FIVE_HOUR_MS = 5 * 60 * 60 * 1000;
const SEVEN_DAY_MS = 7 * 24 * 60 * 60 * 1000;

export type OpencodeCost = {
  /** Summed assistant-message cost (USD) over the trailing 5h / 7d. */
  fiveHour: number;
  sevenDay: number;
};

type CostRow = { total: number | null };

let costStmt: Statement<CostRow, [{ $after: number }]> | null = null;

function ensureStmt(): Statement<CostRow, [{ $after: number }]> | null {
  if (costStmt) return costStmt;
  const db = openDb();
  if (!db) return null;
  try {
    costStmt = db.query<CostRow, { $after: number }>(
      `SELECT COALESCE(SUM(json_extract(data,'$.cost')), 0) AS total
       FROM message
       WHERE json_extract(data,'$.role') = 'assistant'
         AND time_created > $after`,
    ) as unknown as Statement<CostRow, [{ $after: number }]>;
    return costStmt;
  } catch (err) {
    log.warn("opencode cost: prepare stmt failed", { err: String(err) });
    return null;
  }
}

/**
 * Sum OpenCode spend over the trailing 5h and 7d. Returns null when the
 * DB is unavailable; zeros are a valid result (no spend in the window).
 */
export function readOpencodeCost(nowMs: number): OpencodeCost | null {
  const stmt = ensureStmt();
  if (!stmt) return null;
  try {
    const five = stmt.get({ $after: nowMs - FIVE_HOUR_MS })?.total ?? 0;
    const seven = stmt.get({ $after: nowMs - SEVEN_DAY_MS })?.total ?? 0;
    return { fiveHour: five, sevenDay: seven };
  } catch (err) {
    log.debug("opencode cost query failed", { err: String(err) });
    return null;
  }
}
