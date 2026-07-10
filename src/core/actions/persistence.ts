import {
  existsSync,
  readFileSync,
  renameSync,
  writeFileSync,
} from "node:fs";
import { join } from "node:path";

import type { ActionLine } from "../harness/claude/events.ts";
import { type DoneSentinel, readDoneFile } from "./tail.ts";
import { createLogger } from "../logger.ts";
import type { ActionMeta, ActionRun } from "./types.ts";

const log = createLogger("[actions]");

export function readMetaSafe(runDir: string): ActionMeta | null {
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

export function readDoneSafe(runDir: string): DoneSentinel | null {
  return readDoneFile(join(runDir, "done.json"));
}

/** Atomic-ish write: write to a temp sibling and rename. fs.watch
 *  consumers that fire mid-write get a complete object rather than a
 *  half-written one. */
export function writeMetaSync(runDir: string, meta: ActionMeta): void {
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
export function writeDoneSentinelBestEffort(
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

export function materializeRun(
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
    ...(meta.autoFireKeys && meta.autoFireKeys.length > 0
      ? { autoFireKeys: meta.autoFireKeys }
      : {}),
  };
}

export { log };
