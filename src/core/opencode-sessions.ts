/**
 * Stable wt-friendly names for OpenCode sessions. OpenCode owns the
 * authoritative session IDs and may store a useful `session.title`, but
 * fresh/untitled sessions can show generic timestamp titles. Wt assigns
 * stable fallback names (`primary`, `2`, `3`, ...) per slug so the TUI
 * stays recognizable even before OpenCode has a meaningful title.
 *
 * Shape: `{ <slug>: { <session-id>: "<friendly-name>" } }`.
 */
import { existsSync, mkdirSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

import { withFileLock } from "./locks.ts";
import { createLogger } from "./logger.ts";

const STATE_FILE = join(homedir(), ".cache", "wt", "opencode-sessions.json");
const log = createLogger("[opencode-sessions]");

type FileShape = Record<string, Record<string, string>>;

function readFile(): FileShape {
  if (!existsSync(STATE_FILE)) return {};
  try {
    const parsed = JSON.parse(readFileSync(STATE_FILE, "utf8")) as unknown;
    if (!parsed || typeof parsed !== "object") return {};
    const out: FileShape = {};
    for (const [slug, rawNames] of Object.entries(
      parsed as Record<string, unknown>,
    )) {
      if (!rawNames || typeof rawNames !== "object") continue;
      const names: Record<string, string> = {};
      for (const [sessionId, name] of Object.entries(
        rawNames as Record<string, unknown>,
      )) {
        if (typeof name === "string" && name.length > 0) {
          names[sessionId] = name;
        }
      }
      if (Object.keys(names).length > 0) out[slug] = names;
    }
    return out;
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), { file: STATE_FILE });
    return {};
  }
}

function writeFile(shape: FileShape): void {
  try {
    mkdirSync(dirname(STATE_FILE), { recursive: true });
    const tmp = `${STATE_FILE}.${process.pid}.tmp`;
    writeFileSync(tmp, `${JSON.stringify(shape, null, 2)}\n`);
    renameSync(tmp, STATE_FILE);
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), { file: STATE_FILE });
    throw err;
  }
}

function withNamesLock<T>(fn: () => T): T {
  return withFileLock("__opencode_names__", fn);
}

function assignMissingNames(
  existing: Readonly<Record<string, string>>,
  sessionIds: readonly string[],
): { names: Record<string, string>; changed: boolean } {
  const names = { ...existing };
  const used = new Set(Object.values(names));
  let changed = false;

  for (const sessionId of sessionIds) {
    if (names[sessionId]) continue;
    let name: string;
    if (!used.has("primary")) {
      name = "primary";
    } else {
      let n = 2;
      while (used.has(`${n}`)) n++;
      name = `${n}`;
    }
    names[sessionId] = name;
    used.add(name);
    changed = true;
  }
  return { names, changed };
}

export function reconcileOpencodeNames(
  slug: string,
  sessionIds: readonly string[],
): Readonly<Record<string, string>> {
  try {
    return withNamesLock(() => {
      const shape = readFile();
      const { names, changed } = assignMissingNames(
        shape[slug] ?? {},
        sessionIds,
      );
      if (changed) {
        shape[slug] = names;
        writeFile(shape);
      }
      return names;
    });
  } catch (err) {
    log.error(err instanceof Error ? err : String(err), { file: STATE_FILE });
    return assignMissingNames({}, sessionIds).names;
  }
}

export function clearOpencodeNames(slug: string): void {
  withNamesLock(() => {
    const shape = readFile();
    if (!(slug in shape)) return;
    delete shape[slug];
    writeFile(shape);
  });
}

export function reapOpencodeNames(liveSlugs: ReadonlySet<string>): void {
  withNamesLock(() => {
    const shape = readFile();
    let changed = false;
    for (const slug of Object.keys(shape)) {
      if (!liveSlugs.has(slug)) {
        delete shape[slug];
        changed = true;
      }
    }
    if (changed) writeFile(shape);
  });
}
