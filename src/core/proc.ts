import { config } from "./config.ts";

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
};

/**
 * Run a subprocess, capture stdout/stderr, never throw. Missing
 * binaries and timeouts surface as `exitCode < 0`.
 */
export async function run(argv: string[], opts: RunOptions = {}): Promise<RunResult> {
  const { cwd = config.paths.mainClone, input, timeoutMs, env } = opts;
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

  try {
    const [stdout, stderr, exitCode] = await Promise.all([
      new Response(proc.stdout).text(),
      new Response(proc.stderr).text(),
      proc.exited,
    ]);
    return { stdout, stderr, exitCode };
  } finally {
    if (timer) clearTimeout(timer);
  }
}

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
function sanitizeLine(line: string): string {
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
