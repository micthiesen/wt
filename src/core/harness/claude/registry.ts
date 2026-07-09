/**
 * Live per-process state files that Claude Code writes for every
 * running interactive session at `~/.claude/sessions/<pid>.json`. The
 * file is rewritten on every status transition (busy / idle / waiting /
 * shell) plus a slow heartbeat while busy, which makes it the fastest
 * "is claude doing something right now" signal we can read without
 * subscribing to a binary daemon socket. `waiting` (CC 2.1.145+) means
 * claude is blocked mid-turn on a human, with a `waitingFor` reason like
 * "permission prompt". `shell` means a background shell/task is running
 * while the turn is otherwise done. The full enum is exactly
 * `["busy","shell","idle","waiting"]` (verified against the CC binary),
 * so `unknown` below is purely a future-proofing net.
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

import { createLogger } from "../../logger.ts";

const log = createLogger("[claude-registry]");

export const REGISTRY_DIR = join(homedir(), ".claude", "sessions");

export type RegistryStatus = "busy" | "shell" | "idle" | "waiting" | "unknown";

export type RegistrySession = {
  pid: number;
  sessionId: string;
  cwd: string;
  /** `--name` flag value; null when unnamed (the implicit primary). */
  name: string | null;
  status: RegistryStatus;
  /** Reason claude is blocked, present only when status is "waiting"
   *  (e.g. "permission prompt"). Null otherwise. */
  waitingFor: string | null;
  kind: string;
  entrypoint: string;
  startedAt: number;
  updatedAt: number;
};

function asNumber(v: unknown): number | null {
  return typeof v === "number" && Number.isFinite(v) ? v : null;
}

/** Accept only positive integer pids. `process.kill(0, 0)` targets the
 *  current process group; `process.kill(-N, 0)` targets a group. Either
 *  yields a phantom "live" entry against a malformed/stale registry file. */
function asPid(v: unknown): number | null {
  return typeof v === "number" && Number.isInteger(v) && v > 0 ? v : null;
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
  const pid = asPid(obj.pid);
  const sessionId = obj.sessionId;
  const cwd = obj.cwd;
  const rawStatus = obj.status;
  if (
    pid === null ||
    typeof sessionId !== "string" ||
    typeof cwd !== "string" ||
    typeof rawStatus !== "string"
  ) {
    return null;
  }
  // A present-but-unrecognized status (a future CC status we don't know
  // yet) is kept as "unknown" rather than dropping the whole entry —
  // dropping it would make a live session fall through to the jsonl-tail
  // path and mislabel as "abandoned". Only an absent/non-string status
  // (genuinely malformed file) is rejected above.
  const status: RegistryStatus =
    rawStatus === "busy" ||
    rawStatus === "shell" ||
    rawStatus === "idle" ||
    rawStatus === "waiting"
      ? rawStatus
      : "unknown";
  return {
    pid,
    sessionId,
    cwd,
    name: typeof obj.name === "string" ? obj.name : null,
    status,
    waitingFor: typeof obj.waitingFor === "string" ? obj.waitingFor : null,
    kind: typeof obj.kind === "string" ? obj.kind : "",
    entrypoint: typeof obj.entrypoint === "string" ? obj.entrypoint : "",
    startedAt: asNumber(obj.startedAt) ?? 0,
    updatedAt: asNumber(obj.updatedAt) ?? 0,
  };
}

/**
 * True iff `pid` names a process visible to us. `EPERM` means the
 * process exists but is owned by a different uid — still alive from our
 * perspective. `ESRCH` is the no-such-process signal. Anything else
 * (transient kernel weirdness, EFAULT, …) we treat as alive and log,
 * so a one-shot syscall failure doesn't silently disappear a real
 * session from the picker.
 *
 * Pid-recycling note: this can't distinguish "the process that wrote
 * this file" from "a new process that reused the pid". Stale files
 * left after a crash can briefly point at an unrelated process. The
 * downside is bounded — the stale `status` reads as last-known until
 * the real claude rewrites or the file ages out via cleanup.
 */
function pidAlive(pid: number): boolean {
  try {
    process.kill(pid, 0);
    return true;
  } catch (err) {
    const code = (err as NodeJS.ErrnoException).code;
    if (code === "EPERM") return true;
    if (code === "ESRCH") return false;
    log.warn("pid liveness check failed", { pid, code: code ?? "?" });
    return true;
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

/** Coalesce a burst of file events into one `onChange` call. Claude
 *  rewrites its state file on every transition + a slow heartbeat; with
 *  several concurrent sessions FSEvents can dispatch many events per
 *  status flip. Trailing-edge wait so we fire after the burst settles. */
const WATCH_DEBOUNCE_MS = 100;

/**
 * Subscribe to file events in the registry dir. The callback fires
 * after a brief debounce; callers re-read lazily (the cheap path is a
 * no-op if nothing observable changed). Returns a disposer; safe to
 * call multiple times.
 *
 * fs.watch can throw ENOENT if the dir doesn't exist yet (fresh
 * machine, never ran claude). FSEvents can also emit a runtime `error`
 * after a successful start (dir replaced, perms changed). Both paths
 * log and fall through to the polling backstop on the query rather
 * than crashing the TUI.
 */
export function watchRegistry(onChange: () => void): () => void {
  let timer: ReturnType<typeof setTimeout> | null = null;
  let disposed = false;
  const trigger = (): void => {
    if (timer !== null) clearTimeout(timer);
    timer = setTimeout(() => {
      timer = null;
      if (!disposed) onChange();
    }, WATCH_DEBOUNCE_MS);
  };
  try {
    const w = watch(REGISTRY_DIR, trigger);
    w.on("error", (err) => {
      log.warn("fs.watch error", { err: String(err), dir: REGISTRY_DIR });
    });
    return () => {
      disposed = true;
      if (timer !== null) {
        clearTimeout(timer);
        timer = null;
      }
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
