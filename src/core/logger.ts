/**
 * Structured logger.
 *
 * Two channels per logger:
 *   - `debug`/`info`/`warn`/`error` — file-only. Cheap, async, never
 *     blocks. Writes structured ctx (`{key: value}`) inline as JSON.
 *   - `event.{info,ok,warn,err,dim}` — file AND the activity pane (when
 *     a sink is registered). Replaces the old free emitters in
 *     `tui/activity-log.ts`. Each event-pane line is a strict subset of the
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
import { Deferred, Effect, Queue } from "effect";

import { config } from "./config.ts";
import { operationErrors } from "./errors.ts";

export type EventKind = "info" | "ok" | "warn" | "err" | "dim";

/**
 * Which pane feed an event belongs to. `firehose` is the classic
 * everything-log; `attention` is the curated feed the bottom pane
 * shows by default — status transitions, needs-you signals, things
 * worth interrupting a scan for. Errors emitted on the firehose still
 * surface in the attention VIEW (level-based include); the channel
 * only marks what was deliberately curated.
 */
export type EventChannel = "attention" | "firehose";

export type EventRecord = {
  level: EventKind;
  channel: EventChannel;
  source: string;
  text: string;
};

export type EventSink = (e: EventRecord) => void;

/**
 * Toast control for an event/attention emit. Attention-channel emits
 * flash as a footer toast BY DEFAULT (attention means "worth
 * interrupting a scan", and the toast is that interruption); firehose
 * emits opt in with `{ toast: true }`. Either way the flag rides the
 * emit, so a background toast is structurally guaranteed to have a
 * pane line backing it — toasts are ephemeral, the feeds are the
 * record. Inert when no toast sink is registered (CLI runs).
 */
export type EmitOpts = { toast?: boolean };

export type ToastRecord = { level: EventKind; source: string; text: string };
export type ToastSink = (t: ToastRecord) => void;

export interface Logger {
  debug(msg: string, ctx?: object): void;
  info(msg: string, ctx?: object): void;
  warn(msg: string, ctx?: object): void;
  error(msg: string | Error, ctx?: object): void;
  event: {
    info(text: string, opts?: EmitOpts): void;
    ok(text: string, opts?: EmitOpts): void;
    warn(text: string, opts?: EmitOpts): void;
    err(text: string, opts?: EmitOpts): void;
    dim(text: string, opts?: EmitOpts): void;
  };
  /**
   * Curated high-signal feed (see `EventChannel`). No `dim` on
   * purpose — if it's dim it isn't attention-worthy. File lines are
   * tagged `ATTN` (vs `EVENT`) so `grep ' ATTN '` reconstructs what
   * the attention pane showed.
   */
  attention: {
    info(text: string, opts?: EmitOpts): void;
    ok(text: string, opts?: EmitOpts): void;
    warn(text: string, opts?: EmitOpts): void;
    err(text: string, opts?: EmitOpts): void;
  };
  child(source: string): Logger;
}

const RETAIN_DAYS = 14;
const LVL_PAD = 5;

/** ISO-8601 width: `Date.prototype.toISOString()` is always 24 chars. */
export const TS_LEN = 24;
/** Width of the `EVENT`/`ATTN ` event-channel tag in a pane-line record. */
export const TAG_PAD = 5;
/** `kind.padEnd(KIND_PAD)` width in a pane-line record (`info`/`ok`/`warn`/`err`/`dim`). */
export const KIND_PAD = 4;
/** `source.padEnd(SRC_PAD)` width in a pane-line record; unpadded (not truncated) when the source is longer. */
export const SRC_PAD = 16;

let sink: EventSink | null = null;
let toastSink: ToastSink | null = null;
// A single Effect Queue worker serializes writes so lines land in order.
// Each `appendFile` reopens the file, so daily rollover is automatic.
let initialized = false;

const io = operationErrors("logger");

type LogCommand =
  | { readonly _tag: "Write"; readonly path: string; readonly line: string }
  | { readonly _tag: "Flush"; readonly done: Deferred.Deferred<void> };

let commandQueue: Queue.Queue<LogCommand> | null = null;

function ensureLoggerWorker(): Queue.Queue<LogCommand> {
  if (commandQueue) return commandQueue;
  const queue = Effect.runSync(Queue.unbounded<LogCommand>());
  commandQueue = queue;
  const run = Effect.forever(Queue.take(queue).pipe(Effect.flatMap((command) => {
    if (command._tag === "Flush") return Deferred.succeed(command.done, undefined);
    return io.promise(`write ${command.path}`, () => appendFile(command.path, command.line, "utf8")).pipe(
      Effect.catch(() => Effect.void),
    );
  })));
  Effect.runFork(run);
  return queue;
}

export function setEventSink(fn: EventSink | null): void {
  sink = fn;
}

export function setToastSink(fn: ToastSink | null): void {
  toastSink = fn;
}

const flush = Effect.fnUntraced(function* () {
  const queue = ensureLoggerWorker();
  const done = yield* Deferred.make<void>();
  yield* Queue.offer(queue, { _tag: "Flush", done });
  yield* Deferred.await(done);
});

/** Await every queued write before a CLI or daemon exits. */
export const flushLogger: Effect.Effect<void> = flush();

export function createLogger(source: string): Logger {
  return {
    debug: (msg, ctx) => writeFile("DEBUG", source, msg, ctx),
    info: (msg, ctx) => writeFile("INFO", source, msg, ctx),
    warn: (msg, ctx) => writeFile("WARN", source, msg, ctx),
    error: (msg, ctx) => writeError(source, msg, ctx),
    event: {
      info: (text, opts) => emit("info", source, text, "firehose", opts),
      ok: (text, opts) => emit("ok", source, text, "firehose", opts),
      warn: (text, opts) => emit("warn", source, text, "firehose", opts),
      err: (text, opts) => emit("err", source, text, "firehose", opts),
      dim: (text, opts) => emit("dim", source, text, "firehose", opts),
    },
    attention: {
      info: (text, opts) => emit("info", source, text, "attention", opts),
      ok: (text, opts) => emit("ok", source, text, "attention", opts),
      warn: (text, opts) => emit("warn", source, text, "attention", opts),
      err: (text, opts) => emit("err", source, text, "attention", opts),
    },
    child: (sub) => createLogger(sub),
  };
}

function emit(
  kind: EventKind,
  source: string,
  text: string,
  channel: EventChannel,
  opts?: EmitOpts,
): void {
  // Multiline text would corrupt the daily-log line format and overflow
  // the activity pane's fixed one-row-per-event layout. Split into one
  // record per line so each gets its own ts/source/kind prefix in both
  // sinks; preserve leading whitespace so indented blocks stay readable.
  const tag = channel === "attention" ? "ATTN " : "EVENT";
  const lines = splitEventLines(text).map(sanitizeControl);
  for (const line of lines) {
    appendLine(
      `${ts()} ${tag} ${kind.padEnd(KIND_PAD)} ${source.padEnd(SRC_PAD)} ${line}\n`,
    );
    if (sink) {
      try {
        sink({ level: kind, channel, source, text: line });
      } catch {
        // Sink errors must not break logging.
      }
    }
  }
  // Toast the FIRST line only — the footer is a single row; the full
  // text is in the pane feed this emit just wrote.
  const wantToast = opts?.toast ?? channel === "attention";
  if (wantToast && toastSink && lines.length > 0) {
    try {
      toastSink({ level: kind, source, text: lines[0]! });
    } catch {
      // Sink errors must not break logging.
    }
  }
}

function splitEventLines(text: string): string[] {
  if (!text.includes("\n") && !text.includes("\r")) return [text];
  // Split on all three line-terminator forms so a bare `\r` (e.g. progress
  // output from `gh`/`git`) doesn't survive into the pane and reset the
  // cursor. Empty segments (consecutive separators, leading/trailing) are
  // dropped — calling `event.X("\n")` legitimately emits nothing.
  return text.split(/\r\n|\r|\n/).filter((line) => line.length > 0);
}

/**
 * Replace control bytes so the daily file stays grep-able TEXT. One
 * NUL anywhere (a raw stack section key once leaked its "\0stack:"
 * prefix into an event line) flips BSD grep into binary mode and
 * silently breaks `grep ' EVENT '` over the whole file — the exact
 * debugging path the docs recommend. Tabs and \n pass through (emit
 * splits \n out beforehand; writeFile indents it deliberately); \r is
 * replaced here as the backstop for the non-split writeFile path.
 */
function sanitizeControl(text: string): string {
  // eslint-disable-next-line no-control-regex
  return text.replace(/[\0-\x08\x0B-\x1F\x7F]/g, "�");
}

function writeFile(level: string, source: string, msg: string, ctx?: object): void {
  const ctxStr = ctx && Object.keys(ctx).length > 0 ? ` ${safeJson(ctx)}` : "";
  // Indent continuation lines so multi-line stack traces stay readable
  // under `tail -F` without swallowing the next record's timestamp.
  // (ctx is JSON.stringify'd, which already escapes control bytes.)
  const cleaned = sanitizeControl(msg);
  const safeMsg = cleaned.includes("\n")
    ? cleaned.replaceAll("\n", "\n        ")
    : cleaned;
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
    return JSON.stringify(ctx, (key, value) => {
      if (typeof value !== "number" || !Number.isFinite(value)) return value;
      if (/(?:Ms|_ms|milliseconds)$/.test(key)) return Math.round(value);
      if (/(?:Seconds|_seconds)$/.test(key)) return Math.round(value * 10) / 10;
      return value;
    });
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
  const queue = ensureLoggerWorker();
  Effect.runSync(Queue.offer(queue, { _tag: "Write", path: dailyPath(), line }));
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
