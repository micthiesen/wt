/**
 * Live per-process state files that Claude Code writes for every
 * running interactive session at `~/.claude/sessions/<pid>.json`. The
 * file is rewritten on every status transition (busy ↔ idle) plus a
 * slow heartbeat while busy, which makes it the fastest "is claude
 * doing something right now" signal we can read without subscribing
 * to a binary daemon socket.
 *
 * Claude doesn't clean up the file on SIGKILL, so we filter dead pids
 * via `kill -0`. The schema is undocumented — we pin to a small subset
 * of fields and tolerate missing/extra keys rather than parsing
 * strictly.
 *
 * `watchRegistry` exposes the dir to an fs.watch consumer; the TUI
 * runtime invalidates `claudeRegistryQuery` on every event so status
 * flips appear without waiting for the polling backstop.
 */
import { readdirSync, readFileSync, watch } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

import { createLogger } from "./logger.ts";

const log = createLogger("[claude-registry]");

export const REGISTRY_DIR = join(homedir(), ".claude", "sessions");

export type RegistryStatus = "busy" | "idle";

export type RegistrySession = {
  pid: number;
  sessionId: string;
  cwd: string;
  /** `--name` flag value; null when unnamed (the implicit primary). */
  name: string | null;
  status: RegistryStatus;
  kind: string;
  entrypoint: string;
  startedAt: number;
  updatedAt: number;
};

function asNumber(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

function parseEntry(path: string): RegistrySession | null {
  let raw: string;
  try {
    raw = readFileSync(path, "utf8");
  } catch {
    return null;
  }
  let obj: Record<string, unknown>;
  try {
    const parsed = JSON.parse(raw);
    if (!parsed || typeof parsed !== "object") return null;
    obj = parsed as Record<string, unknown>;
  } catch {
    return null;
  }
  const pid = asNumber(obj.pid);
  const sessionId = obj.sessionId;
  const cwd = obj.cwd;
  const status = obj.status;
  if (
    pid === null ||
    typeof sessionId !== "string" ||
    typeof cwd !== "string" ||
    (status !== "busy" && status !== "idle")
  ) {
    return null;
  }
  return {
    pid,
    sessionId,
    cwd,
    name: typeof obj.name === "string" ? obj.name : null,
    status,
    kind: typeof obj.kind === "string" ? obj.kind : "",
    entrypoint: typeof obj.entrypoint === "string" ? obj.entrypoint : "",
    startedAt: asNumber(obj.startedAt) ?? 0,
    updatedAt: asNumber(obj.updatedAt) ?? 0,
  };
}

/**
 * True iff `pid` names a process visible to us. `EPERM` means the
 * process exists but is owned by a different uid — still alive from our
 * perspective, so we keep it. `ESRCH` is the no-such-process signal.
 */
function pidAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    const code = (err as NodeJS.ErrnoException).code;
    return code === "EPERM";
  }
}

/**
 * Snapshot of every live claude session on the machine. One readdir +
 * one parse + one `kill -0` per file. Stale files (claude crashed
 * without cleanup) are silently dropped.
 */
export function readRegistry(): RegistrySession[] {
  let names: string[];
  try {
    names = readdirSync(REGISTRY_DIR);
  } catch (err) {
    const code = (err as NodeJS.ErrnoException).code;
    if (code === "ENOENT") return [];
    log.warn("readdir failed", { err: String(err) });
    return [];
  }
  const out: RegistrySession[] = [];
  for (const name of names) {
    if (!name.endsWith(".json")) continue;
    const parsed = parseEntry(join(REGISTRY_DIR, name));
    if (!parsed) continue;
    if (!pidAlive(parsed.pid)) continue;
    out.push(parsed);
  }
  return out;
}

/**
 * Subscribe to file events in the registry dir. The callback fires on
 * every create/change/delete; callers debounce by re-reading lazily
 * (the cheap path is a no-op if nothing observable changed). Returns a
 * disposer; safe to call multiple times.
 *
 * fs.watch can throw ENOENT if the dir doesn't exist yet (fresh
 * machine, never ran claude). We log and return a no-op disposer in
 * that case — the polling backstop on the query covers eventual
 * appearance.
 */
export function watchRegistry(onChange: () => void): () => void {
  try {
    const w = watch(REGISTRY_DIR, () => onChange());
    return () => {
      try {
        w.close();
      } catch {
        // already closed
      }
    };
  } catch (err) {
    log.warn("fs.watch failed", { err: String(err), dir: REGISTRY_DIR });
    return () => {};
  }
}
