/**
 * Wt-private tmux server: one isolated tmux universe (`-L wt`) hosting
 * one detachable `claude` session per worktree slug. Survives wt
 * restart; never visible to the user as tmux (no status bar, no
 * keybindings the user touches).
 *
 * The user-facing entry point is `tui/claude-session.ts`, which
 * suspends the renderer and shells out to `attachOrCreate` here. Status
 * polling lives in `tmuxSessionsQuery` (state/queries.ts).
 *
 * # Why TMUX gets stripped from the spawn env
 *
 * `claude` inspects `$TMUX` directly and force-downgrades RGB output to
 * 256-color when set, regardless of `COLORTERM`, `FORCE_COLOR`, or the
 * inner terminfo's `RGB` capability. tmux sets `TMUX` automatically for
 * its own children, so the only way out is `env -u TMUX -u TMUX_PANE
 * claude` at the new-session boundary. Without this the logo renders
 * peach instead of orange.
 *
 * # Config-change detection
 *
 * tmux loads its config exactly once at server start. Updating
 * `tmux.conf` while a server is running is a no-op for that server. We
 * compare the rendered config to the on-disk version; if it differs,
 * we kill the server before attaching so the new config takes effect.
 */
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

import { wtSessionArgs } from "./claude.ts";
import { config } from "./config.ts";
import { createLogger } from "./logger.ts";

const log = createLogger("[tmux]");

/** Socket name (`-L`) for the wt-private tmux server. */
export const TMUX_SOCKET = "wt";

/**
 * Kinds of session this module manages. `claude` is the persistent
 * F12 conversation; `diff` is the F11 git-diff TUI; `shell` is the
 * F10 plain login shell. Each gets its own tmux session per slug —
 * `<slug>` for claude, `<slug>-diff` for diff, `<slug>-shell` for
 * shell — so they coexist without interfering.
 */
export type SessionKind = "claude" | "diff" | "shell";

const SUFFIX: Record<Exclude<SessionKind, "claude">, string> = {
  diff: "-diff",
  shell: "-shell",
};

function sessionName(slug: string, kind: SessionKind): string {
  return kind === "claude" ? slug : `${slug}${SUFFIX[kind]}`;
}

/** Strip a kind suffix from a session name to recover the bare slug. */
function bareSlug(name: string): string {
  for (const suffix of Object.values(SUFFIX)) {
    if (name.endsWith(suffix)) return name.slice(0, -suffix.length);
  }
  return name;
}

/** Path to the generated tmux.conf. */
function configDir(): string {
  const dir = join(homedir(), ".cache", "wt");
  mkdirSync(dir, { recursive: true });
  return dir;
}

/**
 * Render the wt-private tmux config. Notable choices:
 *  - `status off` + `set-titles off`: no tmux chrome anywhere.
 *  - `alternate-screen off`: tmux fakes alt-screen for inner programs
 *    instead of switching the outer terminal's buffer, which removes
 *    the flash on enter/exit between opentui's alt-screen and tmux's.
 *  - `escape-time 0`: kills the 500ms ESC delay that breaks claude's
 *    keybindings.
 *  - `mouse on` + `focus-events on`: silences claude's "add this to
 *    your tmux.conf" advice and gives wheel-scroll + IDE focus.
 *  - Truecolor declared two ways (modern `terminal-features :RGB` +
 *    legacy `terminal-overrides :Tc`) — different tools check
 *    different paths.
 *  - `unbind C-b` + F10/F11/F12 all bound to detach-client: kill the
 *    tmux prefix entirely; each F-key is a single-press detach.
 *    Symmetric with the wt-side bindings — whichever F-key the user
 *    pressed to enter (F10 shell, F11 diff, F12 claude) takes them
 *    back out. Binding all three to detach in any context is
 *    harmless: an accidental F11 inside a claude session just exits,
 *    same as F12.
 */
export function buildConfig(): string {
  const outerTerm = process.env.TERM ?? "xterm-256color";
  return `set -g status off
set -g alternate-screen off
set -g set-titles off
set -sg escape-time 0
set -g mouse on
set -g focus-events on
set -g default-terminal "tmux-256color"
set -as terminal-features ",${outerTerm}:RGB"
set -ag terminal-overrides ",${outerTerm}:Tc"
set -ag update-environment "COLORTERM"
unbind C-b
bind-key -n F10 detach-client
bind-key -n F11 detach-client
bind-key -n F12 detach-client
`;
}

/**
 * Write the config to disk if it differs from what's already there.
 * Returns the path and whether the file actually changed — callers use
 * `changed` to decide whether to kill+restart the server (see header).
 */
export function writeConfig(): { path: string; changed: boolean } {
  const path = join(configDir(), "tmux.conf");
  const next = buildConfig();
  let prev = "";
  try {
    prev = readFileSync(path, "utf8");
  } catch {
    // first run
  }
  const changed = prev !== next;
  if (changed) writeFileSync(path, next, "utf8");
  return { path, changed };
}

/**
 * Kill the entire wt tmux server (every session). Idempotent — exits
 * 0 when no server is running, after warning to stderr we discard.
 */
export async function killServer(): Promise<void> {
  const proc = Bun.spawn(["tmux", "-L", TMUX_SOCKET, "kill-server"], {
    stdout: "ignore",
    stderr: "ignore",
  });
  await proc.exited;
}

/**
 * Kill one worktree's claude session. Idempotent — silently no-ops
 * when the session doesn't exist or the server isn't running. Other
 * kinds are unaffected; use `killDiffSession` / `killShellSession`
 * for those, or `killAllSessionsFor` to drop every kind at once.
 */
export async function killSession(slug: string): Promise<void> {
  await killByName(sessionName(slug, "claude"));
}

/** Kill one worktree's diff session. Idempotent. */
export async function killDiffSession(slug: string): Promise<void> {
  await killByName(sessionName(slug, "diff"));
}

/** Kill one worktree's shell session. Idempotent. */
export async function killShellSession(slug: string): Promise<void> {
  await killByName(sessionName(slug, "shell"));
}

/**
 * Kill every kind of session for a slug (claude, diff, shell). Used
 * by destroy paths so none linger with cwd inside a half-deleted
 * worktree.
 */
export async function killAllSessionsFor(slug: string): Promise<void> {
  await Promise.allSettled([
    killSession(slug),
    killDiffSession(slug),
    killShellSession(slug),
  ]);
}

async function killByName(name: string): Promise<void> {
  const proc = Bun.spawn(
    ["tmux", "-L", TMUX_SOCKET, "kill-session", "-t", `=${name}`],
    { stdout: "ignore", stderr: "ignore" },
  );
  await proc.exited;
}

/**
 * Bare slug sets for the live sessions of each kind, partitioned so
 * the indicators and the F10/F11/F12 kill-confirm hints can read
 * each independently. One CLI call regardless of worktree count.
 *
 * Server-not-running exits non-zero with a "no server running"
 * stderr; we map that to empty sets rather than throwing — it's the
 * steady state when no worktree has been entered yet.
 */
export async function listSessions(): Promise<{
  claude: Set<string>;
  diff: Set<string>;
  shell: Set<string>;
}> {
  const all = await listAllSessionsRaw();
  const claude = new Set<string>();
  const diff = new Set<string>();
  const shell = new Set<string>();
  for (const name of all) {
    if (name.endsWith(SUFFIX.diff)) {
      diff.add(name.slice(0, -SUFFIX.diff.length));
    } else if (name.endsWith(SUFFIX.shell)) {
      shell.add(name.slice(0, -SUFFIX.shell.length));
    } else {
      claude.add(name);
    }
  }
  return { claude, diff, shell };
}

/**
 * Every session name on our private tmux server, including the
 * `<slug>-diff` ones. Used by the reaper and by `attachOrCreate`'s
 * post-detach existence check, which need exact-name matching
 * regardless of kind.
 */
async function listAllSessionsRaw(): Promise<Set<string>> {
  const proc = Bun.spawn(
    [
      "tmux",
      "-L",
      TMUX_SOCKET,
      "list-sessions",
      "-F",
      "#{session_name}",
    ],
    { stdout: "pipe", stderr: "ignore" },
  );
  const [out, code] = await Promise.all([
    new Response(proc.stdout).text(),
    proc.exited,
  ]);
  if (code !== 0) return new Set();
  const names = out
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);
  return new Set(names);
}

export type AttachResult =
  | { kind: "exited"; code: number | null }
  | { kind: "detached" }
  | { kind: "spawn-failed"; reason: string };

/**
 * Attach to (or create) the worktree's session and run the configured
 * inner program (claude for `kind: "claude"`, the user's diff TUI for
 * `kind: "diff"`). Stdio is inherited so tmux owns the user's
 * terminal until they detach (F11/F12) or the inner program exits.
 * Returns once tmux's client process exits.
 *
 * Caller is responsible for suspending/resuming any UI renderer around
 * this call — this function makes no assumptions about who owns the
 * terminal before/after.
 */
export async function attachOrCreate(opts: {
  slug: string;
  cwd: string;
  kind: SessionKind;
}): Promise<AttachResult> {
  const { slug, cwd, kind } = opts;
  const name = sessionName(slug, kind);
  const { path: configPath, changed } = writeConfig();
  if (changed) {
    log.info("config changed, killing server before attach", { slug, kind });
    await killServer();
  }

  let proc: Bun.Subprocess;
  try {
    // Claude resume-vs-create resolves at attach time. Re-checking on
    // every attach (rather than caching) means an externally-deleted
    // jsonl (claude project purge, manual rm) recovers cleanly: next
    // attach sees the file gone, switches back to --session-id with
    // the same UUID, and recreates. The diff branch shells out to the
    // configured command via the user's login shell so PATH/init
    // (pyenv, mise, …) apply. The shell branch is just the login
    // shell with no command — exit (Ctrl+D / `exit`) ends the session.
    const userShell = process.env.SHELL || "bash";
    const innerArgs =
      kind === "claude"
        ? ["claude", ...wtSessionArgs(cwd)]
        : kind === "diff"
          ? [userShell, "-lc", config.diff.command]
          : [userShell, "-l"];
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
        ...innerArgs,
      ],
      {
        cwd,
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
  return stillRunning ? { kind: "detached" } : { kind: "exited", code };
}

/**
 * Reconcile sessions against a live slug set. Kills any session
 * (claude or diff) whose underlying slug isn't in `liveSlugs` — covers
 * the case where a worktree was destroyed (in this wt run or a prior
 * one) without our session-kill hook firing. The bare slug is derived
 * by stripping the diff suffix when present so both kinds are reaped
 * for a removed worktree. Errors are swallowed; an orphaned session is
 * a worse outcome than blocking startup.
 */
export async function reapOrphanedSessions(
  liveSlugs: ReadonlySet<string>,
): Promise<void> {
  let sessions: Set<string>;
  try {
    sessions = await listAllSessionsRaw();
  } catch {
    return;
  }
  const orphans = [...sessions].filter((s) => !liveSlugs.has(bareSlug(s)));
  if (orphans.length === 0) return;
  log.info(`reaping ${orphans.length} orphaned tmux session(s)`, {
    orphans,
  });
  await Promise.allSettled(orphans.map((name) => killByName(name)));
}
