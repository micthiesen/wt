/**
 * Per-action recent-value history. Backs the `arg_prompt` picker — when
 * an action declares `arg_prompt`, the picker (after the action is
 * picked from the `!` menu) shows the most-recent values the user has
 * supplied for that action, so common ones become a 1-keystroke pick
 * instead of a re-type / re-paste.
 *
 * # Storage
 *
 * `~/.cache/wt/action-history.json`. Single file, atomic-ish writes via
 * temp-and-rename. Shape:
 *
 *   {
 *     "seed-company": [
 *       { "value": "acme-123", "label": "Acme Co (42 files)", "ts": 1736… },
 *       ...
 *     ],
 *     ...
 *   }
 *
 * `label` is set by `recordRun` when the action's `label_extract`
 * regex matched a line of the run's captured output; if not, only
 * `value` carries through and the picker renders the value verbatim.
 *
 * # Behavior
 *
 * - `recordRun(actionId, value, label?)` prepends a new entry, dedupes
 *   by value (any prior entry with the same value is removed first so
 *   the just-used value floats to the top), then truncates the list to
 *   `MAX_PER_ACTION`. Idempotent against a missing file (just creates).
 * - `recentValues(actionId)` reads the file and returns the entries
 *   for one action in most-recent-first order. Synchronous; the file
 *   is tiny and only opened on picker-open.
 *
 * Errors (parse, write, ENOENT) are swallowed with a debug log: a
 * missing or corrupt history file is fine, the picker just shows an
 * empty list and the user types a fresh value.
 */
import {
  closeSync,
  existsSync,
  mkdirSync,
  openSync,
  readFileSync,
  renameSync,
  unlinkSync,
  writeSync,
} from "node:fs";
import { dirname, join } from "node:path";

import { config } from "./config.ts";
import { createLogger } from "./logger.ts";

const log = createLogger("[action-history]");

/** Cap per action. Big enough to cover a few weeks of company churn; *  small enough that the picker stays scannable. */
const MAX_PER_ACTION = 10;

const HISTORY_PATH = join(dirname(config.paths.cacheDb), "action-history.json");

export type HistoryEntry = {
  value: string;
  /** From the action's `label_extract` regex against the run's
   *  captured output. Null when no extractor is configured or nothing
   *  matched. */
  label: string | null;
  /** Last-used timestamp (ms). The newest entry sits at index 0. */
  ts: number;
};

type Store = Record<string, HistoryEntry[]>;

function readStore(): Store {
  if (!existsSync(HISTORY_PATH)) return {};
  try {
    const raw = readFileSync(HISTORY_PATH, "utf8");
    const parsed = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object" || Array.isArray(parsed)) return {};
    return parsed as Store;
  } catch (err) {
    log.warn("read failed; treating as empty", {
      err: err instanceof Error ? err.message : String(err),
    });
    return {};
  }
}

function writeStore(store: Store): void {
  try {
    mkdirSync(dirname(HISTORY_PATH), { recursive: true });
    const tmp = `${HISTORY_PATH}.${process.pid}.tmp`;
    const fd = openSync(tmp, "w");
    try {
      writeSync(fd, JSON.stringify(store, null, 2));
    } finally {
      closeSync(fd);
    }
    renameSync(tmp, HISTORY_PATH);
  } catch (err) {
    log.warn("write failed; history not persisted", {
      err: err instanceof Error ? err.message : String(err),
    });
    // Best-effort cleanup of a half-written temp file. ENOENT is fine.
    try {
      unlinkSync(`${HISTORY_PATH}.${process.pid}.tmp`);
    } catch {
      /* ignore */
    }
  }
}

export function recentValues(actionId: string): readonly HistoryEntry[] {
  const store = readStore();
  return store[actionId] ?? [];
}

/**
 * Prepend `value` to the action's history. If the same value already
 * exists in the list, it's removed first (so a re-use floats to the
 * top instead of growing duplicates). `label` is optional; passing
 * `null` keeps any existing label intact (e.g. early write at launch
 * before the extractor has anything to scan, refined later).
 */
export function recordRun(
  actionId: string,
  value: string,
  label: string | null,
): void {
  if (!value) return;
  const store = readStore();
  const existing = store[actionId] ?? [];
  const prior = existing.find((e) => e.value === value);
  const next: HistoryEntry = {
    value,
    label: label ?? prior?.label ?? null,
    ts: Date.now(),
  };
  const rest = existing.filter((e) => e.value !== value);
  store[actionId] = [next, ...rest].slice(0, MAX_PER_ACTION);
  writeStore(store);
}
