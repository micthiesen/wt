import { mkdirSync, readFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

import { config } from "../config.ts";
import { getHarness, type Harness } from "../harness/index.ts";
import { createLogger } from "../logger.ts";
import { shellLogPath } from "../shell-tail.ts";
import { killServer, resolveDiffCommand } from "./admin.ts";
import { writeConfig } from "./config.ts";
import {
  harnessIdForKind,
  sessionSwitchTarget,
  sessionName,
  type SessionKind,
  type SessionShortcut,
  shQuote,
  TMUX_SOCKET,
} from "./naming.ts";
import { listAllSessionsRaw } from "./process.ts";

const log = createLogger("[tmux]");

export type AttachResult =
  | { kind: "exited"; code: number | null; stderr: string | null }
  | { kind: "detached" }
  | { kind: "switch"; target: SessionShortcut }
  | { kind: "spawn-failed"; reason: string };

/**
 * Working directory for every tmux *client* process we spawn. The first
 * client to touch the socket forks the tmux server, and the daemonized
 * server keeps that client's cwd for its whole life. If that cwd is a
 * worktree that later gets destroyed, the server is left sitting in a
 * deleted directory — and tmux then silently fails to apply `-c` on new
 * panes (its save-and-restore `open(".")` fails), so every new session
 * starts in the dead directory. Bun-based inner programs (claude) call
 * getcwd() at startup and die instantly with a bare ENOENT. Pane cwd
 * always comes from `-c`, never from the client, so pinning clients to
 * $HOME costs nothing and makes the server's cwd immortal.
 */
export function tmuxClientCwd(): string {
  return homedir();
}

/**
 * Where the per-session stderr capture file lives. Stable name keyed
 * on the tmux session name so re-attaches share the same file the
 * original `bash` is still appending to. Created lazily on first attach.
 */
export function sessionsDir(): string {
  const dir = join(homedir(), ".cache", "wt", "sessions");
  mkdirSync(dir, { recursive: true });
  return dir;
}

// CSI + OSC + bare ESC sequences. Same regex as core/proc.ts; copied
// here rather than imported because that module pulls in TanStack
// Query plumbing we don't want in the tmux layer.
// eslint-disable-next-line no-control-regex
const ANSI_RE = /\x1b\[[0-9;?]*[ -/]*[@-~]|\x1b\][^\x07\x1b]*(?:\x07|\x1b\\)|\x1b[@-Z\\-_]/g;
const STDERR_TAIL_LIMIT = 2000;

/**
 * Scrub captured stderr down to something readable in the activity
 * pane: drop ANSI/control-char noise, collapse whitespace-only lines,
 * keep only the tail so a long-running session that leaked stderr
 * doesn't flood the pane on exit.
 */
function scrubStderr(raw: string): string | null {
  // eslint-disable-next-line no-control-regex
  const cleaned = raw
    .replace(ANSI_RE, "")
    .replace(/\r/g, "")
    // eslint-disable-next-line no-control-regex
    .replace(/[\x00-\x08\x0b\x0c\x0e-\x1f\x7f]/g, "");
  const lines = cleaned
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);
  if (lines.length === 0) return null;
  let out = lines.join("\n");
  if (out.length > STDERR_TAIL_LIMIT) {
    out = `…${out.slice(-STDERR_TAIL_LIMIT)}`;
  }
  return out;
}

/**
 * Attach to (or create) the worktree's session and run the configured
 * inner program (claude for `kind: "claude"`, the user's diff TUI for
 * `kind: "diff"`). Stdio is inherited so tmux owns the user's
 * terminal until they detach (F11/F12) or the inner program exits.
 * Returns once tmux's client process exits.
 *
 * Action sessions are intentionally excluded — they're not user-
 * attachable and have no inner-program-from-config story; their
 * lifecycle lives in `core/tmux/action-sessions.ts`.
 *
 * Caller is responsible for suspending/resuming any UI renderer around
 * this call — this function makes no assumptions about who owns the
 * terminal before/after.
 */
export async function attachOrCreate(opts: {
  slug: string;
  cwd: string;
  kind: Exclude<SessionKind, "action">;
  /**
   * For AI harness kinds (`claude` / `codex` / `opencode`). Claude:
   * `null` → primary tmux slot, string → named additional session
   * (`<slug>~<name>`). Codex / OpenCode ignore the managed name for
   * the tmux name (single-tmux-per-slug) but pass it through to
   * `harness.buildArgs` so the impl can surface it where relevant.
   */
  managedName?: string | null;
  /**
   * Harness session id to resume. Only the AI kinds honor this. Pass
   * `null` to spawn fresh.
   */
  resumeSessionId?: string | null;
  /**
   * For Claude primary only — label shown in `/resume`. Defaults to
   * "primary". Session-slot entries (`.` / `,` bindings) pass the
   * slot's label here so the conversation is recognizable by name;
   * ignored for named claude sessions and for other harnesses.
   */
  claudeDisplayName?: string;
  /**
   * Resolved diff base ref for `{{base}}` substitution in
   * `config.diff.command`. Required by callers using a base-aware diff
   * command (the shipped default `revdiff --vim-motion --compact {{base}}`); ignored
   * for non-diff kinds and for diff commands that don't reference
   * `{{base}}`. Substitution is verbatim — caller is responsible for
   * providing a ref that's safe to splice into a shell-quoted command
   * (the ref is double-quoted at spawn time).
   */
  base?: string;
}): Promise<AttachResult> {
  const {
    slug,
    cwd,
    kind,
    managedName,
    resumeSessionId,
    claudeDisplayName,
    base,
  } = opts;
  const harnessId = harnessIdForKind(kind);
  const harness: Harness | null = harnessId ? getHarness(harnessId) : null;
  const managedNameNorm = harness ? (managedName ?? null) : null;
  const name = sessionName(slug, kind, managedNameNorm);
  const shortcut: SessionShortcut = harness
    ? "harness"
    : kind === "diff"
      ? "diff"
      : "shell";
  const { path: configPath, changed } = writeConfig();
  if (changed) {
    log.info("config changed, killing server before attach", { slug, kind });
    await killServer();
  }
  // Stable per-session-name file for the inner program's stderr.
  // The bash wrapper below sets this up via `2>` so claude/diff/shell
  // startup errors (e.g. claude rejecting --session-id when the jsonl
  // already exists) survive past the tmux pty teardown — without this
  // they flash to the user's terminal during the spawn-and-die window
  // and disappear, leaving only "exited (0)" in the activity pane.
  const stderrPath = join(sessionsDir(), `${name}.err`);

  // AI harness branches delegate argv to the registered impl. Each
  // impl decides resume-vs-create at attach time (claude: presence of
  // its jsonl; codex / opencode: presence of `resumeSessionId`) so
  // recovery from external deletes is automatic — the next attach
  // re-evaluates and picks the right form. The diff branch shells out
  // to the configured command via the user's login shell so PATH/init
  // (pyenv, mise, …) apply. The shell branch is just the login shell
  // with no command — exit (Ctrl+D / `exit`) ends the session.
  const userShell = process.env.SHELL || "bash";
  let innerArgs: string[];
  if (harness) {
    innerArgs = harness.buildArgs({
      wtPath: cwd,
      managedName: managedNameNorm,
      resumeSessionId: resumeSessionId ?? null,
      displayLabel: claudeDisplayName,
    });
  } else if (kind === "diff") {
    innerArgs = [userShell, "-lc", resolveDiffCommand(config.diff.command, base)];
  } else {
    innerArgs = [userShell, "-l"];
  }

  // Shell: pre-create the session detached and chain `pipe-pane` to a
  // per-slug log so every byte the shell writes is captured for the
  // bottom-pane tail (`core/shell-tail.ts`). Doing this before the
  // attach call closes the would-be race where output written between
  // session-create and pipe-pane attach gets dropped.
  //
  // The pre-create must NOT use `new-session -A`: when the session
  // already exists (re-entering F10), `-A` switches to attach
  // semantics, which needs a controlling tty — but this spawn has
  // none (`stdout: "ignore"`), so tmux fails with "open terminal
  // failed: not a terminal" and the tail silently never attaches.
  // Branch instead:
  //  - fresh session: `new-session -d` chained with `pipe-pane` in one
  //    command so no shell output is lost between create and pipe.
  //  - existing session: just re-run `pipe-pane -o` (the `-o` makes it
  //    a no-op when the pipe's already attached); the shell's been
  //    running, so there's no create→pipe race to close.
  // claude/diff don't need any of this — claude has its own jsonl
  // tail, and diff is a TUI we don't surface as an output.
  if (kind === "shell") {
    const shellLog = shellLogPath(slug);
    // pipe-pane runs its argument through /bin/sh -c, so the path
    // has to survive shell parsing — `homedir()` can contain spaces
    // (macOS "My Name" accounts) even though the slug can't.
    const quotedLog = shQuote(shellLog);
    // `>` not `>>` so a destroy-and-recreate of the same slug doesn't
    // seed the new tail with the prior session's lines.
    const pipePaneArgs = ["pipe-pane", "-o", "-t", name, `cat > ${quotedLog}`];
    const alreadyRunning = (await listAllSessionsRaw().catch(() => new Set<string>())).has(name);
    const setupArgs = alreadyRunning
      ? ["tmux", "-L", TMUX_SOCKET, "-f", configPath, ...pipePaneArgs]
      : [
          "tmux",
          "-L",
          TMUX_SOCKET,
          "-f",
          configPath,
          "new-session",
          "-d",
          "-s",
          name,
          "-c",
          cwd,
          "env",
          "-u",
          "TMUX",
          "-u",
          "TMUX_PANE",
          "bash",
          "-c",
          'p="$1"; shift; exec "$@" 2> "$p"',
          "_wt_wrap",
          stderrPath,
          ...innerArgs,
          ";",
          ...pipePaneArgs,
        ];
    const setup = Bun.spawn(setupArgs, {
      cwd: tmuxClientCwd(),
      stdout: "ignore",
      stderr: "pipe",
    });
    const [setupCode, setupErr] = await Promise.all([
      setup.exited,
      new Response(setup.stderr).text(),
    ]);
    if (setupCode !== 0) {
      // Falling through to the regular attach below means the user
      // still gets a working shell, but the bottom-pane tail will sit
      // at "waiting for shell session output…" for this session's
      // lifetime. Surface the actual stderr so the failure is
      // diagnosable instead of just logged as an exit code.
      log.warn("shell pre-create + pipe-pane failed; tail disabled", {
        slug,
        code: setupCode,
        stderr: setupErr.trim() || null,
      });
    }
  }

  // Record which physical shortcut owns this session. The tmux config uses
  // this option to make the owning key detach while the other F-keys request
  // an in-place switch. Set it on every attach so sessions created by older
  // wt versions are upgraded automatically.
  const tag = Bun.spawn(
    [
      "tmux",
      "-L",
      TMUX_SOCKET,
      "-f",
      configPath,
      "set-option",
      "-t",
      name,
      "@wt-shortcut",
      shortcut,
    ],
    { cwd: tmuxClientCwd(), stdout: "ignore", stderr: "ignore" },
  );
  await tag.exited;

  let proc: Bun.Subprocess;
  try {
    proc = Bun.spawn(
      [
        "tmux",
        "-L",
        TMUX_SOCKET,
        "-f",
        configPath,
        "new-session",
        "-A",
        "-s",
        name,
        "-c",
        cwd,
        // See header: claude downgrades to 256-color when $TMUX is set.
        "env",
        "-u",
        "TMUX",
        "-u",
        "TMUX_PANE",
        // Wrap the inner program in a tiny bash that redirects stderr
        // to `stderrPath` before exec'ing. `exec` keeps the process
        // tree flat (the inner program inherits bash's PID, no extra
        // hop). `new-session -A` only runs this command on creation —
        // subsequent re-attaches share the file the original bash is
        // still appending to. The redirect target is passed as $1 so
        // we never have to shell-escape paths. Stdout stays on tmux's
        // pty so the inner UI renders normally.
        "bash",
        "-c",
        'p="$1"; shift; exec "$@" 2> "$p"',
        "_wt_wrap",
        stderrPath,
        ...innerArgs,
        ";",
        "set-option",
        "-t",
        name,
        "@wt-shortcut",
        shortcut,
      ],
      {
        // NOT the worktree — see tmuxClientCwd. The session's start
        // directory comes from `-c` above; the client cwd only matters
        // as the potential birth cwd of the tmux server.
        cwd: tmuxClientCwd(),
        stdin: "inherit",
        stdout: "inherit",
        // Pipe (not inherit) so tmux client noise like
        // `[detached (from session X)]` and `[exited]` doesn't leak
        // into the user's terminal. opentui's alt-screen hides them
        // while wt is running, but they stick around in the main-screen
        // buffer and become visible after wt exits — multiplied by every
        // F12 cycle the user did. Claude's UI is unaffected; it flows
        // through tmux's pty into our inherited stdout.
        stderr: "pipe",
        env: {
          ...process.env,
          TERM: process.env.TERM ?? "xterm-256color",
          COLORTERM: process.env.COLORTERM ?? "truecolor",
          FORCE_COLOR: process.env.FORCE_COLOR ?? "3",
        },
      },
    );
  } catch (err) {
    const reason = err instanceof Error ? err.message : String(err);
    log.error("spawn failed", { slug, kind, reason });
    return { kind: "spawn-failed", reason };
  }

  // Drain stderr to the file log so genuine tmux errors aren't
  // silently lost. Resolves when the process closes the stream.
  const drainStderr = (async () => {
    const stream = proc.stderr as ReadableStream<Uint8Array> | undefined;
    if (!stream) return;
    try {
      const text = await new Response(stream).text();
      const trimmed = text.trim();
      if (trimmed) log.debug("tmux stderr", { slug, kind, text: trimmed });
    } catch (err) {
      log.warn("stderr drain failed", {
        slug,
        kind,
        err: err instanceof Error ? err.message : String(err),
      });
    }
  })();

  const code = await proc.exited;
  await drainStderr;
  // tmux client exits with 0 on detach AND on session-end (inner
  // program exit). Distinguish by re-querying the raw set: if the
  // session still exists, the user detached; if not, the inner program
  // exited and tmux cleaned up.
  const sessions = await listAllSessionsRaw();
  const stillRunning = sessions.has(name);
  if (stillRunning) {
    const target = sessionSwitchTarget(code);
    if (target) return { kind: "switch", target };
    return { kind: "detached" };
  }
  // Inner program died. Read whatever it wrote to stderr so the
  // caller can surface the actual reason instead of just "exited (N)".
  // ENOENT here just means the bash redirect never produced any
  // output, which is the steady state for a clean exit.
  let stderrText: string | null = null;
  try {
    const raw = readFileSync(stderrPath, "utf8");
    stderrText = scrubStderr(raw);
  } catch {
    // no stderr file — bash never wrote anything, or it was already swept
  }
  return { kind: "exited", code, stderr: stderrText };
}
