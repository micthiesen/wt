/**
 * Per-worktree tailer for the wt-managed F10 shell tmux session. Symmetric
 * to `core/session-tail.ts` (which tails F12 claude's jsonl), but the
 * source is a plain pipe-pane log file rather than structured stream-json.
 *
 * # Capture pipeline
 *
 * `attachOrCreate(kind: "shell")` chains a `tmux pipe-pane -o` after
 * `new-session -A -d` so every byte the shell writes to its pane gets
 * written to `shellLogPath(slug)`. The pipe shell uses `>` (not `>>`),
 * so the log truncates on first session-create and a destroy-and-recreate
 * of the same slug doesn't seed the new tail with the prior session's
 * lines. From there it's the same fs.watch + debounce + delta-read
 * pattern as session-tail.
 *
 * # ANSI stripping & carriage returns
 *
 * Pipe-pane captures the raw pty stream — full of colors, cursor moves,
 * OSC title sets, alt-screen toggles. We strip the lot with the same
 * regex that `core/tmux.ts`'s stderr scrubber uses, then collapse `\r`
 * by keeping only the segment after the last CR per line (so progress
 * bars like `npm install`'s render as their final state instead of as
 * one giant smear of overwrites).
 *
 * # Line discipline
 *
 * Pipe-pane has no notion of "line". We split the captured byte stream
 * on `\n`, hold the trailing fragment in `pending` for the next read,
 * and ring-buffer to `MAX_BUFFERED_LINES` so a runaway prompt loop
 * can't OOM the TUI.
 */
import {
  type FSWatcher,
  closeSync,
  existsSync,
  mkdirSync,
  openSync,
  readdirSync,
  readSync,
  rmSync,
  statSync,
  watch,
} from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

import { MAX_BUFFERED_LINES } from "./claude-events.ts";
import { createLogger } from "./logger.ts";

const log = createLogger("[shell-tail]");

/** How many trailing bytes of the pipe-pane log to seed from on first ensure. */
const SEED_TAIL_BYTES = 32 * 1024;
/** Coalesce window for fs.watch bursts. */
const READ_DEBOUNCE_MS = 80;
/**
 * Backstop polling cadence — matches session-tail / action-tail. Bun's
 * `fs.watch` on macOS can silently miss appends; the poll re-uses the
 * delta-read pipeline (`readDelta` short-circuits when `size === lastByte`)
 * so the idle cost is one stat per tail per tick.
 */
const POLL_INTERVAL_MS = 3_000;

// CSI + OSC + bare ESC — same regex as core/tmux.ts's stderr scrubber.
// eslint-disable-next-line no-control-regex
const ANSI_RE = /\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]/g;
// Other C0 control characters we want to drop (BEL, backspace, etc.) —
// keep \t (renders as space-runs) and let \r flow through to the
// progress-collapse step before final cleanup.
// eslint-disable-next-line no-control-regex
const CTRL_RE = /[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]/g;

// Powerline glyphs and nerd-font symbols used as prompt segment
// separators and theme icons (powerlevel10k, starship, …). U+E000..
// U+E0FF spans the BMP private-use area where these glyphs live; real
// command output essentially never contains characters in this range,
// so any line that does is a prompt redraw.
const POWERLINE_GLYPHS_RE = /[-]/;
// Prompt filler — `·` (U+00B7) and `.` runs, used by powerlevel10k to
// pad the prompt out to the right edge. 6+ in a row is a strong signal:
// real output doesn't hit that threshold without being deliberately
// cosmetic.
const PROMPT_FILLER_RE = /[·.]{6,}/;
// zsh's missing-newline marker: an inverted `%` at the start of a line
// when the previous command's output didn't end with `\n`. After ANSI
// + control stripping it lands as a bare `%`. Drop only when that's
// the entire line so output containing literal `%` isn't filtered.
const ZSH_MISSING_NL_RE = /^%$/;

// Bracketed-paste mode markers. zsh wraps user input in
// `\e[?2004h ... \e[?2004l`, so a line containing `[?2004l` carries
// a typed command in its preceding bytes — surface it as `> <cmd>`.
const PASTE_START = "\x1b[?2004h";
const PASTE_END = "\x1b[?2004l";
// Strip the prompt indicator off the front of the reconstructed input:
// optional leading whitespace + box-drawing chars + an arrow-style
// marker + space. Covers powerlevel10k's `╰─❯ `, starship's `❯ `,
// bash-style `$ ` / `% ` / `# `, and similar. Anchored — bare `>` /
// `$` / `%` mid-command stay intact.
const PROMPT_PREFIX_RE = /^[\s─-▟]*[❯>›→»$%#]\s+/;
// Cap on reconstructed input length. Some interactive TUIs (e.g. hunk)
// toggle bracketed-paste mode internally so the entire TUI session
// reconstructs as one giant "command"; drop those.
const MAX_RECONSTRUCT_LEN = 200;

function shellLogDir(): string {
  return join(homedir(), ".cache", "wt", "shell-logs");
}

/** Stable per-slug log path under wt's cache dir. */
export function shellLogPath(slug: string): string {
  const dir = shellLogDir();
  mkdirSync(dir, { recursive: true });
  return join(dir, `${slug}.log`);
}

/**
 * Best-effort delete of a slug's pipe-pane log file. Called from the
 * destroy path so the file doesn't outlive the worktree. Errors are
 * swallowed — the startup reap covers anything left behind.
 */
export function removeShellLog(slug: string): void {
  const path = join(shellLogDir(), `${slug}.log`);
  try {
    rmSync(path, { force: true });
  } catch (err) {
    log.warn("shell log delete failed", {
      slug,
      err: err instanceof Error ? err.message : String(err),
    });
  }
}

/**
 * Drop `<slug>.log` files whose slug isn't in `liveSlugs`. Catches
 * logs orphaned by external `git worktree remove`, by destroys that
 * skipped the in-process delete, or by pre-fix wt versions that never
 * cleaned up. Errors are swallowed.
 */
export function reapShellLogs(liveSlugs: ReadonlySet<string>): void {
  const dir = shellLogDir();
  if (!existsSync(dir)) return;
  let names: string[];
  try {
    names = readdirSync(dir);
  } catch {
    return;
  }
  let removed = 0;
  for (const name of names) {
    if (!name.endsWith(".log")) continue;
    const slug = name.slice(0, -".log".length);
    if (liveSlugs.has(slug)) continue;
    try {
      rmSync(join(dir, name), { force: true });
      removed++;
    } catch (err) {
      log.warn("shell log reap failed", {
        name,
        err: err instanceof Error ? err.message : String(err),
      });
    }
  }
  if (removed > 0) log.info("reaped shell logs", { removed });
}

export type ShellLine = {
  /** Stable id for React keys; monotonic across the session. */
  id: number;
  text: string;
  ts: number;
};

export type ShellRun = {
  slug: string;
  startedAt: number;
  lines: readonly ShellLine[];
};

type Listener = () => void;

type State = {
  path: string;
  lastByte: number;
  pending: string;
  watcher: FSWatcher | null;
  dirWatcher: FSWatcher | null;
  debounce: Timer | null;
  /** Monotonic line id counter — restarts at 0 per slug. */
  nextId: number;
};

class ShellTailRegistry {
  private runs: ReadonlyMap<string, ShellRun> = new Map();
  private state = new Map<string, State>();
  private listeners = new Set<Listener>();
  private poller: Timer | null = null;

  /**
   * Idempotent. Spins up a tailer for `slug`'s pipe-pane log if not
   * already running. Safe to call on every render-driven reconcile.
   */
  ensure(slug: string): void {
    const path = shellLogPath(slug);
    const existing = this.state.get(slug);
    if (existing) {
      if (existing.path === path) return;
      this.stop(slug);
    }

    const st: State = {
      path,
      lastByte: 0,
      pending: "",
      watcher: null,
      dirWatcher: null,
      debounce: null,
      nextId: 0,
    };
    this.state.set(slug, st);

    const startedAt = Date.now();
    this.commit((m) => m.set(slug, { slug, startedAt, lines: [] }));

    if (existsSync(path)) {
      this.seedAndWatch(slug);
    } else {
      this.watchForCreation(slug);
    }
    this.ensurePoller();
  }

  stop(slug: string): void {
    const st = this.state.get(slug);
    if (!st) return;
    closeSilent(st.watcher);
    closeSilent(st.dirWatcher);
    if (st.debounce) clearTimeout(st.debounce);
    this.state.delete(slug);
    this.commit((m) => {
      m.delete(slug);
    });
    if (this.state.size === 0) this.stopPoller();
  }

  reconcile(liveSlugs: ReadonlySet<string>): void {
    for (const slug of liveSlugs) this.ensure(slug);
    for (const slug of [...this.state.keys()]) {
      if (!liveSlugs.has(slug)) this.stop(slug);
    }
  }

  stopAll(): void {
    for (const slug of [...this.state.keys()]) this.stop(slug);
    this.stopPoller();
  }

  get(slug: string): ShellRun | null {
    return this.runs.get(slug) ?? null;
  }

  getSnapshot = (): ReadonlyMap<string, ShellRun> => this.runs;

  subscribe = (fn: Listener): (() => void) => {
    this.listeners.add(fn);
    return () => {
      this.listeners.delete(fn);
    };
  };

  // ---------- internals ----------

  private notify(): void {
    for (const l of this.listeners) {
      try {
        l();
      } catch {
        // sink errors must not break dispatch
      }
    }
  }

  private commit(mut: (m: Map<string, ShellRun>) => void): void {
    const next = new Map(this.runs);
    mut(next);
    this.runs = next;
    this.notify();
  }

  private ensurePoller(): void {
    if (this.poller) return;
    this.poller = setInterval(() => {
      for (const [slug, st] of this.state) {
        // Skip tails still in `watchForCreation` mode — see session-tail.
        if (st.watcher == null) continue;
        this.scheduleRead(slug);
      }
    }, POLL_INTERVAL_MS);
  }

  private stopPoller(): void {
    if (!this.poller) return;
    clearInterval(this.poller);
    this.poller = null;
  }

  private update(slug: string, mut: (r: ShellRun) => ShellRun): void {
    const cur = this.runs.get(slug);
    if (!cur) return;
    this.commit((m) => m.set(slug, mut(cur)));
  }

  private watchForCreation(slug: string): void {
    const st = this.state.get(slug);
    if (!st) return;
    const dir = join(st.path, "..");
    try {
      st.dirWatcher = watch(dir, { persistent: false }, (_event, filename) => {
        const target = st.path.split("/").pop();
        if (filename != null && filename !== target) return;
        if (!existsSync(st.path)) return;
        closeSilent(st.dirWatcher);
        st.dirWatcher = null;
        this.seedAndWatch(slug);
      });
    } catch (err) {
      log.warn("dir watch failed", { slug, dir, err: errMsg(err) });
      return;
    }
    if (existsSync(st.path)) {
      closeSilent(st.dirWatcher);
      st.dirWatcher = null;
      this.seedAndWatch(slug);
    }
  }

  private seedAndWatch(slug: string): void {
    const st = this.state.get(slug);
    if (!st) return;
    try {
      this.readSeed(slug);
    } catch (err) {
      log.warn("seed read failed", { slug, err: errMsg(err) });
    }
    try {
      st.watcher = watch(st.path, { persistent: false }, () =>
        this.scheduleRead(slug),
      );
    } catch (err) {
      log.warn("file watch failed", { slug, err: errMsg(err) });
    }
  }

  private readSeed(slug: string): void {
    const st = this.state.get(slug);
    if (!st) return;
    let size = 0;
    try {
      size = statSync(st.path).size;
    } catch {
      return;
    }
    if (size === 0) {
      st.lastByte = 0;
      return;
    }
    const start = Math.max(0, size - SEED_TAIL_BYTES);
    const body = readBytes(st.path, start, size - start);
    const segments = body.split("\n");
    // Drop the first fragment if we didn't start at byte 0 — likely partial.
    const startIdx = start === 0 ? 0 : 1;
    // Last segment is the in-progress line; keep it as `pending`.
    const trailing = segments[segments.length - 1] ?? "";
    const completed = segments.slice(startIdx, -1);
    const ts = Date.now();
    const accum: ShellLine[] = [];
    for (const seg of completed) {
      const cleaned = clean(seg);
      if (cleaned === null) continue;
      accum.push({ id: st.nextId++, text: cleaned, ts });
    }
    st.lastByte = size;
    st.pending = trailing;
    const trimmed =
      accum.length > MAX_BUFFERED_LINES
        ? accum.slice(-MAX_BUFFERED_LINES)
        : accum;
    this.update(slug, (r) => ({ ...r, lines: trimmed }));
  }

  private scheduleRead(slug: string): void {
    const st = this.state.get(slug);
    if (!st) return;
    if (st.debounce) return;
    st.debounce = setTimeout(() => {
      st.debounce = null;
      try {
        this.readDelta(slug);
      } catch (err) {
        log.warn("delta read failed", { slug, err: errMsg(err) });
      }
    }, READ_DEBOUNCE_MS);
  }

  private readDelta(slug: string): void {
    const st = this.state.get(slug);
    if (!st) return;
    let size = 0;
    try {
      size = statSync(st.path).size;
    } catch {
      return;
    }
    if (size === st.lastByte) return;
    if (size < st.lastByte) {
      // File shrank — external truncate. Resync.
      st.lastByte = size;
      st.pending = "";
      return;
    }
    const body = readBytes(st.path, st.lastByte, size - st.lastByte);
    st.lastByte = size;
    const combined = st.pending + body;
    const segments = combined.split("\n");
    st.pending = segments.pop() ?? "";
    const ts = Date.now();
    const newLines: ShellLine[] = [];
    for (const seg of segments) {
      const cleaned = clean(seg);
      if (cleaned === null) continue;
      newLines.push({ id: st.nextId++, text: cleaned, ts });
    }
    if (newLines.length === 0) return;
    this.update(slug, (r) => {
      const merged = [...r.lines, ...newLines];
      const lines =
        merged.length > MAX_BUFFERED_LINES
          ? merged.slice(-MAX_BUFFERED_LINES)
          : merged;
      return { ...r, lines };
    });
  }
}

/**
 * Reconstruct the user's typed command from the bracketed-paste region
 * of a raw shell-log line. zsh-syntax-highlighting redraws the line on
 * every keystroke with SGR colour codes, autosuggestion ghost text, and
 * backspace-rewinds; a minimal cursor simulator that interprets BS,
 * CR, and the cursor / erase CSI sequences (ignoring SGR) reproduces
 * the final state of what the user actually typed.
 *
 * We don't try to be a full terminal — `\e[D` (cursor-back), `\e[C`
 * (cursor-forward), `\e[K` (erase-in-line), `\e[P` (delete-chars), and
 * `\e[G` (cursor-horizontal-absolute) cover the redraw vocabulary zsh
 * emits in practice. Everything else (SGR, mode set/reset, OSC) is
 * scanned for length and skipped.
 */
function reconstructInput(raw: string): string {
  const buf: string[] = [];
  let pos = 0;
  let i = 0;
  const len = raw.length;
  while (i < len) {
    const c = raw[i]!;
    if (c === "\x1b") {
      if (raw[i + 1] === "[") {
        let j = i + 2;
        while (j < len && "<=>?".includes(raw[j]!)) j++;
        let params = "";
        while (j < len && /[0-9;]/.test(raw[j]!)) {
          params += raw[j];
          j++;
        }
        while (j < len) {
          const cc = raw.charCodeAt(j);
          if (cc >= 0x20 && cc <= 0x2f) j++;
          else break;
        }
        const final = raw[j];
        if (final === "D") {
          const n = params ? Math.max(1, parseInt(params, 10)) : 1;
          pos = Math.max(0, pos - n);
        } else if (final === "C") {
          const n = params ? Math.max(1, parseInt(params, 10)) : 1;
          pos = Math.min(buf.length, pos + n);
        } else if (final === "K") {
          const mode = parseInt(params || "0", 10);
          if (mode === 0) buf.length = pos;
          else if (mode === 2) {
            buf.length = 0;
            pos = 0;
          }
        } else if (final === "P") {
          const n = params ? Math.max(1, parseInt(params, 10)) : 1;
          buf.splice(pos, n);
        } else if (final === "G") {
          const n = params ? Math.max(1, parseInt(params, 10)) : 1;
          pos = Math.max(0, n - 1);
        }
        // SGR ('m'), mode set/reset ('h'/'l'), save/restore ('s'/'u'),
        // and other CSI finals: no buffer effect, just skip.
        i = j + 1;
      } else if (raw[i + 1] === "]") {
        // OSC: scan to ST (ESC \) or BEL.
        let j = i + 2;
        while (j < len) {
          if (raw[j] === "\x07") {
            j++;
            break;
          }
          if (raw[j] === "\x1b" && raw[j + 1] === "\\") {
            j += 2;
            break;
          }
          j++;
        }
        i = j;
      } else {
        i += 2;
      }
    } else if (c === "\b") {
      pos = Math.max(0, pos - 1);
      i++;
    } else if (c === "\r") {
      pos = 0;
      i++;
    } else if (c.charCodeAt(0) < 0x20 || c === "\x7f") {
      i++;
    } else {
      buf[pos] = c;
      pos++;
      i++;
    }
  }
  return buf.join("").trimEnd();
}

/**
 * Strip ANSI + collapse carriage returns + drop control chars + filter
 * prompt redraws. Returns `null` for lines that should not surface in
 * the activity pane (empty after cleanup, prompt artifacts).
 *
 * Lines containing `\e[?2004l` carry a user-typed command in their
 * bracketed-paste region: extract via `reconstructInput`, prepend `> `,
 * and short-circuit the rest of the pipeline. The command output that
 * follows the paste-end is on a subsequent log line so it flows through
 * the regular path.
 */
function clean(raw: string): string | null {
  const pasteEnd = raw.indexOf(PASTE_END);
  if (pasteEnd !== -1) {
    const pasteStart = raw.lastIndexOf(PASTE_START, pasteEnd);
    const region =
      pasteStart === -1
        ? raw.slice(0, pasteEnd)
        : raw.slice(pasteStart + PASTE_START.length, pasteEnd);
    const reconstructed = reconstructInput(region);
    if (
      reconstructed.length === 0 ||
      reconstructed.length > MAX_RECONSTRUCT_LEN
    ) {
      return null;
    }
    const cmd = reconstructed.replace(PROMPT_PREFIX_RE, "");
    if (cmd.length === 0) return null;
    return `> ${cmd}`;
  }
  let s = raw.replace(ANSI_RE, "");
  // Drop a single trailing CR from CRLF line endings — pipe-pane
  // captures the raw byte stream, so splitting on \n leaves the \r
  // behind on the previous segment. Without this every interactive
  // command output (ls, git, …) ends in \r and the mid-line CR-collapse
  // below would treat the trailing \r as a progress-bar overwrite and
  // discard the whole line.
  if (s.endsWith("\r")) s = s.slice(0, -1);
  // Mid-line CR collapse: progress bars (`npm install`, `pnpm`, `yarn`)
  // render via repeated \r-overwrites of the same line; pipe-pane
  // captures every redraw, so this drops the intermediate states and
  // keeps only what's after the last remaining CR.
  const cr = s.lastIndexOf("\r");
  if (cr !== -1) s = s.slice(cr + 1);
  s = s.replace(CTRL_RE, "");
  s = s.trimEnd();
  if (s.length === 0) return null;
  // Prompt redraws (powerlevel10k and similar) emit a fresh status
  // line on every keystroke, resize, and command — they'd dominate
  // the pane otherwise. Filter on the three signals that are unique
  // to prompts: powerline-glyph segment separators, dot-fill padding,
  // and zsh's missing-newline marker.
  if (
    POWERLINE_GLYPHS_RE.test(s) ||
    PROMPT_FILLER_RE.test(s) ||
    ZSH_MISSING_NL_RE.test(s)
  ) {
    return null;
  }
  return s;
}

function readBytes(path: string, start: number, len: number): string {
  const fd = openSync(path, "r");
  try {
    const buf = Buffer.alloc(len);
    readSync(fd, buf, 0, len, start);
    return buf.toString("utf8");
  } finally {
    closeSync(fd);
  }
}

function closeSilent(w: FSWatcher | null): void {
  if (!w) return;
  try {
    w.close();
  } catch {
    // best-effort
  }
}

function errMsg(err: unknown): string {
  return err instanceof Error ? err.message : String(err);
}

export const shellTailRegistry = new ShellTailRegistry();
