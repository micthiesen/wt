/**
 * Structured logger.
 *
 * Two channels per logger:
 *   - `debug`/`info`/`warn`/`error` — file-only. Cheap, async, never
 *     blocks. Writes structured ctx (`{key: value}`) inline as JSON.
 *   - `event.{info,ok,warn,err,dim}` — file AND the activity pane (when
 *     a sink is registered). Replaces the old free emitters in
 *     `tui/events.ts`. Each event-pane line is a strict subset of the
 *     file, tagged `EVENT` so `grep '^[^ ]* EVENT'` recovers exactly
 *     what the user saw.
 *
 * One daily file at `<appLogDir>/wt-YYYY-MM-DD.log` (local date — "what
 * happened today" in the user's timezone). Lazy init: nothing touches
 * disk until the first write. Files older than 14 days are unlinked on
 * the same first call.
 *
 * Cross-process safety: every write is a single `appendFile` against
 * the path (O_APPEND under the hood). The TUI, CLI, and detached
 * `_destroy` subprocesses can all write the same daily file without
 * coordination.
 *
 * Logger errors never propagate. A broken disk should not crash the
 * render loop; the user will see other failures soon enough.
 */
import { existsSync, mkdirSync, readdirSync, statSync, unlinkSync } from "node:fs";
import { appendFile } from "node:fs/promises";
import { join } from "node:path";

import { config } from "./config.ts";

export type EventKind = "info" | "ok" | "warn" | "err" | "dim";

export type EventRecord = {
  level: EventKind;
  source: string;
  text: string;
};

export type EventSink = (e: EventRecord) => void;

export interface Logger {
  debug(msg: string, ctx?: object): void;
  info(msg: string, ctx?: object): void;
  warn(msg: string, ctx?: object): void;
  error(msg: string | Error, ctx?: object): void;
  event: {
    info(text: string): void;
    ok(text: string): void;
    warn(text: string): void;
    err(text: string): void;
    dim(text: string): void;
  };
  child(source: string): Logger;
}

const RETAIN_DAYS = 14;
const SRC_PAD = 16;
const LVL_PAD = 5;
const KIND_PAD = 4;

let sink: EventSink | null = null;
// Serialize writes through a promise chain so lines land in order.
// Each `appendFile` reopens the file (fast, ~80μs on macOS APFS) so
// daily rollover is automatic — we just recompute the path per write.
let writeChain: Promise<void> = Promise.resolve();
let initialized = false;

export function setEventSink(fn: EventSink | null): void {
  sink = fn;
}

/** Drain queued writes. Call from TUI/CLI shutdown before `process.exit`. */
export async function flushLogger(): Promise<void> {
  await writeChain;
}

export function createLogger(source: string): Logger {
  return {
    debug: (msg, ctx) => writeFile("DEBUG", source, msg, ctx),
    info: (msg, ctx) => writeFile("INFO", source, msg, ctx),
    warn: (msg, ctx) => writeFile("WARN", source, msg, ctx),
    error: (msg, ctx) => writeError(source, msg, ctx),
    event: {
      info: (text) => emit("info", source, text),
      ok: (text) => emit("ok", source, text),
      warn: (text) => emit("warn", source, text),
      err: (text) => emit("err", source, text),
      dim: (text) => emit("dim", source, text),
    },
    child: (sub) => createLogger(sub),
  };
}

function emit(kind: EventKind, source: string, text: string): void {
  appendLine(
    `${ts()} EVENT ${kind.padEnd(KIND_PAD)} ${source.padEnd(SRC_PAD)} ${text}\n`,
  );
  if (sink) {
    try {
      sink({ level: kind, source, text });
    } catch {
      // Sink errors must not break logging.
    }
  }
}

function writeFile(level: string, source: string, msg: string, ctx?: object): void {
  const ctxStr = ctx && Object.keys(ctx).length > 0 ? ` ${safeJson(ctx)}` : "";
  // Indent continuation lines so multi-line stack traces stay readable
  // under `tail -F` without swallowing the next record's timestamp.
  const safeMsg = msg.includes("\n") ? msg.replaceAll("\n", "\n        ") : msg;
  appendLine(
    `${ts()} ${level.padEnd(LVL_PAD)} ${source.padEnd(SRC_PAD)} ${safeMsg}${ctxStr}\n`,
  );
}

function writeError(source: string, msg: string | Error, ctx?: object): void {
  if (msg instanceof Error) {
    const merged = msg.stack ? { ...(ctx ?? {}), stack: msg.stack } : ctx;
    writeFile("ERROR", source, msg.message, merged);
  } else {
    writeFile("ERROR", source, msg, ctx);
  }
}

function safeJson(ctx: object): string {
  try {
    return JSON.stringify(ctx);
  } catch {
    return '"<unserializable ctx>"';
  }
}

function ts(): string {
  return new Date().toISOString();
}

function todayLocal(): string {
  const d = new Date();
  const y = d.getFullYear();
  const m = String(d.getMonth() + 1).padStart(2, "0");
  const day = String(d.getDate()).padStart(2, "0");
  return `${y}-${m}-${day}`;
}

function dailyPath(): string {
  return join(config.paths.appLogDir, `wt-${todayLocal()}.log`);
}

function appendLine(line: string): void {
  ensureInit();
  const path = dailyPath();
  writeChain = writeChain.then(() => appendFile(path, line, "utf8")).catch(() => {});
}

function ensureInit(): void {
  if (initialized) return;
  initialized = true;
  try {
    mkdirSync(config.paths.appLogDir, { recursive: true });
    sweepOld();
  } catch {
    // Best-effort. If mkdir fails, the first appendFile will fail too —
    // that's swallowed by the writeChain catch above.
  }
}

function sweepOld(): void {
  const dir = config.paths.appLogDir;
  if (!existsSync(dir)) return;
  const cutoff = Date.now() - RETAIN_DAYS * 24 * 60 * 60 * 1000;
  for (const name of readdirSync(dir)) {
    if (!name.startsWith("wt-") || !name.endsWith(".log")) continue;
    const p = join(dir, name);
    try {
      if (statSync(p).mtimeMs < cutoff) unlinkSync(p);
    } catch {
      // Ignore — racy unlink, perms, whatever. Not worth surfacing.
    }
  }
}
