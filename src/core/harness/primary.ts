/**
 * Persisted "primary harness" selection. Single global field — which
 * harness F12 spawns when a worktree has no live session and which one
 * the top-right info bar advertises. TAB cycles through the registered
 * impls; this file is the source of truth across runs.
 *
 * Lives in `~/.cache/wt/harness.json` rather than `wtState.json` so the
 * existing per-slug state file stays focused on section/order.
 */
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

import { createLogger } from "../logger.ts";

import { HARNESSES } from "./registry.ts";
import type { HarnessId } from "./types.ts";

const STATE_FILE = join(homedir(), ".cache", "wt", "harness.json");
const log = createLogger("[harness]");

type FileShape = { primary?: HarnessId };

const DEFAULT_PRIMARY: HarnessId = "claude";

function readFile(): FileShape {
  if (!existsSync(STATE_FILE)) return {};
  try {
    const raw = readFileSync(STATE_FILE, "utf8");
    const parsed = JSON.parse(raw) as unknown;
    if (!parsed || typeof parsed !== "object") return {};
    return parsed as FileShape;
  } catch (err) {
    log.warn("read failed", { err: String(err) });
    return {};
  }
}

function writeFile(shape: FileShape): void {
  try {
    mkdirSync(dirname(STATE_FILE), { recursive: true });
    writeFileSync(STATE_FILE, `${JSON.stringify(shape, null, 2)}\n`);
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), { file: STATE_FILE });
  }
}

/** Current primary harness id. Default `claude` when never set. */
export function readPrimaryHarness(): HarnessId {
  const file = readFile();
  const p = file.primary;
  if (p && HARNESSES.some((h) => h.id === p)) return p;
  return DEFAULT_PRIMARY;
}

/** Persist a new primary selection. Validated against the registry. */
export function writePrimaryHarness(id: HarnessId): void {
  if (!HARNESSES.some((h) => h.id === id)) return;
  writeFile({ primary: id });
}

/** Cycle to the next registered harness. Returns the new primary. */
export function cyclePrimaryHarness(): HarnessId {
  const current = readPrimaryHarness();
  const idx = HARNESSES.findIndex((h) => h.id === current);
  const next = HARNESSES[(idx + 1) % HARNESSES.length]!;
  writePrimaryHarness(next.id);
  return next.id;
}
