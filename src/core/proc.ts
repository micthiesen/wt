import { config } from "./config.ts";

/**
 * Forward an external `AbortSignal` to a local handler. Returns a
 * cleanup function that removes the listener; the listener itself is
 * `{ once: true }`, so this is a belt-and-suspenders cleanup for the
 * non-aborted-yet case. When `signal` is already aborted on entry the
 * handler fires synchronously and no listener is registered.
 *
 * Exported so callers that chain external signals into per-call
 * controllers (AI endpoint fetch, subprocess kill) share one
 * implementation. Without it, the same five-line dance was repeated
 * in two files with subtly different semantics.
 */
export function chainSignal(
  signal: AbortSignal,
  onAbort: () => void,
): () => void {
  if (signal.aborted) {
    onAbort();
    return () => {};
  }
  signal.addEventListener("abort", onAbort, { once: true });
  return () => signal.removeEventListener("abort", onAbort);
}

export type RunResult = {
  stdout: string;
  stderr: string;
  exitCode: number;
};

export type RunOptions = {
  cwd?: string;
  input?: string;
  timeoutMs?: number;
  env?: Record<string, string | undefined>;
  /**
   * Optional cancellation signal. When the signal aborts the spawned
   * process is SIGTERM'd; the awaited stdout/stderr drains then unwind
   * and the function resolves with whatever was captured plus the
   * signal-induced exit code. Pass the queryFn's `signal` so a
   * superseded query (worktree list re-keyed, observer unmounted) stops
   * burning a `gh`/`git` invocation in the background.
   */
  signal?: AbortSignal;
};

/**
 * Run a subprocess, capture stdout/stderr, never throw. Missing
 * binaries and timeouts surface as `exitCode < 0`.
 */
export async function run(argv: string[], opts: RunOptions = {}): Promise<RunResult> {
  const { cwd = config.paths.mainClone, input, timeoutMs, env, signal } = opts;
  const proc = Bun.spawn(argv, {
    cwd,
    stdin: input !== undefined ? "pipe" : "ignore",
    stdout: "pipe",
    stderr: "pipe",
    env: env ? { ...process.env, ...env } : process.env,
  });
  if (input !== undefined && proc.stdin) {
    proc.stdin.write(input);
    proc.stdin.end();
  }

  let timer: Timer | undefined;
  if (timeoutMs) {
    timer = setTimeout(() => proc.kill("SIGKILL"), timeoutMs);
  }

  // Abort plumbing: SIGTERM on signal, but let the drains complete so
  // the caller still gets a structured RunResult instead of an
  // uncaught rejection. If the signal is already aborted, kill
  // immediately (the spawn race window is small but real).
  const cleanupAbort = signal
    ? chainSignal(signal, () => {
        try {
          proc.kill("SIGTERM");
        } catch {
          // proc may already have exited
        }
      })
    : noop;

  try {
    const [stdout, stderr, exitCode] = await Promise.all([
      new Response(proc.stdout).text(),
      new Response(proc.stderr).text(),
      proc.exited,
    ]);
    return { stdout, stderr, exitCode };
  } finally {
    if (timer) clearTimeout(timer);
    cleanupAbort();
  }
}

const noop = (): void => {};

/**
 * Run and return trimmed stdout. Throws on non-zero exit with a
 * message including stderr — matches Python's `subprocess.run(check=True)`.
 */
export async function runOk(argv: string[], opts: RunOptions = {}): Promise<string> {
  const r = await run(argv, opts);
  if (r.exitCode !== 0) {
    const msg = r.stderr.trim() || r.stdout.trim() || `exit ${r.exitCode}`;
    throw new Error(`${argv.join(" ")}: ${msg}`);
  }
  return r.stdout.trimEnd();
}

/** Returns true when the command exits zero. Never throws. */
export async function runQuiet(argv: string[], opts: RunOptions = {}): Promise<boolean> {
  const r = await run(argv, opts);
  return r.exitCode === 0;
}

// Matches CSI (`ESC [ … letter`), OSC (`ESC ] … BEL|ST`), and bare
// two-byte ESC sequences. Enough to scrub the color/cursor noise that
// `pnpm`, `sst`, and friends emit even when stdout isn't a TTY.
// eslint-disable-next-line no-control-regex
const ANSI_RE = /\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]/g;

/**
 * Strip ANSI escapes, collapse in-place `\r` overwrites to the final
 * visible state, and scrub remaining control characters. Tabs in
 * particular confuse OpenTUI's width calc (it counts 1 cell; the real
 * terminal expands to the next tab stop), which cascades into rows
 * overflowing their allocated height and colliding with siblings.
 */
export function sanitizeLine(line: string): string {
  let s = line.replace(ANSI_RE, "");
  const lastCr = s.lastIndexOf("\r");
  if (lastCr >= 0) s = s.slice(lastCr + 1);
  s = s.replace(/\t/g, " ");
  // Drop remaining C0 / DEL control bytes. LF already split upstream.
  // eslint-disable-next-line no-control-regex
  s = s.replace(/[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]/g, "");
  return s;
}

/**
 * Drain a ReadableStream of bytes, splitting on newlines and invoking
 * `onLine` for each complete line. Trailing partial-line content is
 * flushed at end-of-stream.
 */
export async function streamLines(
  stream: ReadableStream<Uint8Array>,
  onLine: (line: string) => void,
): Promise<void> {
  const reader = stream.getReader();
  const decoder = new TextDecoder();
  let buf = "";
  try {
    while (true) {
      const { value, done } = await reader.read();
      if (done) break;
      buf += decoder.decode(value, { stream: true });
      let nl = buf.indexOf("\n");
      while (nl !== -1) {
        onLine(sanitizeLine(buf.slice(0, nl)));
        buf = buf.slice(nl + 1);
        nl = buf.indexOf("\n");
      }
    }
    const tail = buf + decoder.decode();
    if (tail) onLine(sanitizeLine(tail));
  } finally {
    try {
      reader.releaseLock();
    } catch (err) {
      // Lock may already be released if the reader errored out. Safe
      // to ignore — the stream is owned by this function.
      void err;
    }
  }
}

/**
 * Spawn a subprocess, stream stdout+stderr line-by-line through the
 * callback, resolve with the exit code. Lets long-running output surface
 * in the TUI without blocking on `inherit`.
 */
export async function runStreaming(
  argv: string[],
  opts: RunOptions & { onLine?: (line: string) => void } = {},
): Promise<number> {
  // Deliberately ignores `timeoutMs`/`signal` from RunOptions: callers
  // are long-running lifecycle ops (pnpm install, sst remove) where a
  // mid-flight kill leaves worse state than waiting. If a future caller
  // needs cancellation, wire it up here explicitly — don't assume the
  // options work just because the type accepts them.
  const { cwd, env, onLine } = opts;
  const proc = Bun.spawn(argv, {
    cwd,
    stdin: "ignore",
    stdout: "pipe",
    stderr: "pipe",
    env: env ? { ...process.env, ...env } : process.env,
  });
  const emit = onLine ?? (() => {});
  await Promise.all([
    streamLines(proc.stdout, emit),
    streamLines(proc.stderr, emit),
  ]);
  return proc.exited;
}
