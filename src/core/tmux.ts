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

import { killActionSession } from "./action-tmux.ts";
import { wtSessionArgs } from "./claude.ts";
import { config } from "./config.ts";
import { createLogger } from "./logger.ts";
import { shellLogPath } from "./shell-tail.ts";

const log = createLogger("[tmux]");

/** Socket name (`-L`) for the wt-private tmux server. */
export const TMUX_SOCKET = "wt";

/**
 * Slug for the persistent claude session at the wt source repo (the
 * `.` keybinding). Reuses `kind: "claude"`, so the tmux session name
 * is just `wt` and the conversation shows up as `wt` in claude's
 * `/resume` listings. `runtime.tsx` adds this to `live` so the orphan
 * reaper doesn't mistake it for a vanished worktree.
 */
export const WT_SOURCE_SLUG = "wt";

/**
 * Kinds of session this module manages. `claude` is the persistent
 * F12 conversation; `diff` is the F11 git-diff TUI; `shell` is the
 * F10 plain login shell; `action` is a wt-managed action runner
 * (claude `-p` or shell command) supervised by tmux so it survives
 * wt restarts. Each gets its own tmux session per slug — `<slug>`
 * for claude (primary), `<slug>~<name>` for additional named claude
 * sessions, `<slug>-diff` for diff, `<slug>-shell` for shell,
 * `<slug>-action` for action — so they coexist without interfering.
 *
 * Action sessions are not user-attachable and are not driven by the
 * F-key codepath in this module — `core/action-tmux.ts` owns their
 * lifecycle. The kind is registered here so `listSessions` and
 * `reapOrphanedSessions` see them uniformly with the other kinds.
 */
export type SessionKind = "claude" | "diff" | "shell" | "action";

const SUFFIX: Record<Exclude<SessionKind, "claude">, string> = {
  diff: "-diff",
  shell: "-shell",
  action: "-action",
};

/**
 * Separator between slug and a named claude session's user-supplied
 * name in the tmux session name. `~` was picked because it never
 * appears in slugs (derived from branch names — git accepts most
 * ASCII but `~` collides with reflog syntax in practice) and never in
 * validated session names (see `validateSessionName` in
 * `claude-sessions.ts`). Stripping is unambiguous: rightmost `~`
 * splits slug from name.
 */
const CLAUDE_NAMED_SEP = "~";

function sessionName(slug: string, kind: SessionKind, claudeName?: string): string {
  if (kind !== "claude") return `${slug}${SUFFIX[kind]}`;
  return claudeName === undefined ? slug : `${slug}${CLAUDE_NAMED_SEP}${claudeName}`;
}

/**
 * Single-quote a path for safe interpolation into a /bin/sh command
 * string. Used by the pipe-pane redirect target — `homedir()` paths
 * can contain spaces ("My Name" accounts on macOS), so a raw splice
 * would break the redirect. Embedded single quotes are escaped via
 * `'\''` per POSIX shell convention.
 */
function shQuote(s: string): string {
  return `'${s.replace(/'/g, "'\\''")}'`;
}

/**
 * Recover the bare slug from a tmux session name. Strips any kind
 * suffix (`-diff`, `-shell`, `-action`) and any named-claude suffix
 * (`~<name>`). Order matters: a named-claude session like
 * `foo-shell~bar` (theoretically impossible — slugs don't end in
 * `-shell`) would parse as kind=`shell`, slug=`foo`, but our
 * validation forbids `~` in names so this never resolves wrong in
 * practice. We strip the named-claude `~` first since it's the
 * rightmost decoration.
 */
function bareSlug(name: string): string {
  const tildeIdx = name.lastIndexOf(CLAUDE_NAMED_SEP);
  const stripped = tildeIdx >= 0 ? name.slice(0, tildeIdx) : name;
  for (const suffix of Object.values(SUFFIX)) {
    if (stripped.endsWith(suffix)) return stripped.slice(0, -suffix.length);
  }
  return stripped;
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
 *  - `extended-keys on` + `:extkeys` feature: lets tmux distinguish
 *    Shift+Enter from plain Enter so claude's newline shortcut works.
 *    `allow-passthrough on` lets desktop notifications + the progress
 *    bar reach the outer terminal instead of being swallowed by tmux.
 *    All three are the official Anthropic-recommended tmux config.
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
set -g allow-passthrough on
set -s extended-keys on
set -as terminal-features ",${outerTerm}:extkeys"
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
 * Kill one worktree's primary claude session. Idempotent — silently
 * no-ops when the session doesn't exist or the server isn't running.
 * Other kinds and any *named* claude sessions on the same slug are
 * unaffected; use `killClaudeNamedSession` / `killDiffSession` /
 * `killShellSession` for those, or `killAllSessionsFor` to drop
 * every kind at once.
 */
export async function killSession(slug: string): Promise<void> {
  await killByName(sessionName(slug, "claude"));
}

/** Kill one worktree's named (non-primary) claude session. Idempotent. */
export async function killClaudeNamedSession(
  slug: string,
  claudeName: string,
): Promise<void> {
  await killByName(sessionName(slug, "claude", claudeName));
}

/** Kill one worktree's diff session. Idempotent. */
export async function killDiffSession(slug: string): Promise<void> {
  await killByName(sessionName(slug, "diff"));
}

/** Kill one worktree's shell session. Idempotent. */
export async function killShellSession(slug: string): Promise<void> {
  await killByName(sessionName(slug, "shell"));
}

// Action session kills go through `core/action-tmux.ts` —
// `killAllSessionsFor` below imports the sync helper there. Keeping a
// duplicate definition here would invite drift between two sources of
// truth for the same tmux command.

/**
 * Kill every kind of session for a slug (claude primary + every
 * named claude, diff, shell, action). Used by destroy paths so none
 * linger with cwd inside a half-deleted worktree.
 */
export async function killAllSessionsFor(slug: string): Promise<void> {
  // List once and pick out any session whose bareSlug matches —
  // covers primary, named claudes, diff, shell, and action without
  // hardcoding the named-claude list. Action kills go via
  // action-tmux.ts which is synchronous; wrap in Promise.resolve to
  // keep the allSettled shape uniform.
  const all = await listAllSessionsRaw().catch(() => new Set<string>());
  const ours = [...all].filter((n) => bareSlug(n) === slug);
  await Promise.allSettled([
    ...ours.map((n) => killByName(n)),
    Promise.resolve().then(() => killActionSession(slug)),
  ]);
}

const BASE_PLACEHOLDER = "{{base}}";

/** Whether the user's `[diff].command` template depends on the diff base. */
export function diffCommandUsesBase(template: string): boolean {
  return template.includes(BASE_PLACEHOLDER);
}

/**
 * Substitute `{{base}}` in the user's diff command template with the
 * resolved base ref. The ref is wrapped in double quotes so refs
 * containing characters that the user's shell would otherwise expand
 * (e.g. globs in oddly-named local branches) survive intact. Refs
 * starting with `origin/` and ordinary branch names contain only safe
 * characters in practice; the quoting is belt-and-braces.
 *
 * Templates that don't reference `{{base}}` pass through unchanged so
 * users with custom diff commands (`gitu`, `lazygit`, …) keep working.
 * Templates that do reference it but receive no base resolve the
 * placeholder to the empty string so the user's shell surfaces the
 * resulting parse error visibly rather than us silently masking the
 * misuse.
 */
function resolveDiffCommand(template: string, base: string | undefined): string {
  if (!diffCommandUsesBase(template)) return template;
  const ref = base ? `"${base.replaceAll('"', '\\"')}"` : "";
  return template.replaceAll(BASE_PLACEHOLDER, ref);
}

async function killByName(name: string): Promise<void> {
  const proc = Bun.spawn(
    ["tmux", "-L", TMUX_SOCKET, "kill-session", "-t", `=${name}`],
    { stdout: "ignore", stderr: "ignore" },
  );
  await proc.exited;
}

/**
 * One live claude session as seen by tmux. `name = null` is the
 * primary (tmux session name = bare slug); a string is a user-named
 * additional session (tmux session name = `<slug>~<name>`).
 */
export type ClaudeSessionEntry = { slug: string; name: string | null };

/**
 * Bare slug sets (and named-claude entries) for the live sessions of
 * each kind. Partitioned so the indicators, kill-confirm hints, and
 * the sessions picker can each read what they need independently.
 * One CLI call regardless of worktree count.
 *
 * `claude` is now a list of `(slug, name)` because a single worktree
 * can host multiple claude sessions (primary + N named). The legacy
 * `claudeSlugs` set is the unique-slug projection — preserved so
 * "row has any live claude" checks stay a Set lookup.
 *
 * Server-not-running exits non-zero with a "no server running"
 * stderr; we map that to empty sets rather than throwing — it's the
 * steady state when no worktree has been entered yet.
 */
export async function listSessions(): Promise<{
  claude: ClaudeSessionEntry[];
  claudeSlugs: Set<string>;
  diff: Set<string>;
  shell: Set<string>;
  action: Set<string>;
}> {
  const all = await listAllSessionsRaw();
  const claude: ClaudeSessionEntry[] = [];
  const claudeSlugs = new Set<string>();
  const diff = new Set<string>();
  const shell = new Set<string>();
  const action = new Set<string>();
  for (const name of all) {
    if (name.endsWith(SUFFIX.diff)) {
      diff.add(name.slice(0, -SUFFIX.diff.length));
    } else if (name.endsWith(SUFFIX.shell)) {
      shell.add(name.slice(0, -SUFFIX.shell.length));
    } else if (name.endsWith(SUFFIX.action)) {
      action.add(name.slice(0, -SUFFIX.action.length));
    } else {
      const tildeIdx = name.lastIndexOf(CLAUDE_NAMED_SEP);
      if (tildeIdx > 0) {
        const slug = name.slice(0, tildeIdx);
        const claudeName = name.slice(tildeIdx + 1);
        claude.push({ slug, name: claudeName });
        claudeSlugs.add(slug);
      } else {
        claude.push({ slug: name, name: null });
        claudeSlugs.add(name);
      }
    }
  }
  return { claude, claudeSlugs, diff, shell, action };
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
  | { kind: "exited"; code: number | null; stderr: string | null }
  | { kind: "detached" }
  | { kind: "spawn-failed"; reason: string };

/**
 * Where the per-session stderr capture file lives. Stable name keyed
 * on the tmux session name so re-attaches share the same file the
 * original `bash` is still appending to. Created lazily on first attach.
 */
function sessionsDir(): string {
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
 * lifecycle lives in `core/action-tmux.ts`.
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
   * For `kind: "claude"` only. Undefined → the primary session
   * (tmux session name = bare slug; display name in `/resume` =
   * `displayName`). String → a named additional session (tmux name
   * = `<slug>~<claudeName>`; display name = `claudeName`). Ignored
   * for other kinds.
   */
  claudeName?: string;
  /**
   * For `kind: "claude"` only. Label shown in claude's `/resume`
   * picker for the *primary* session. Defaults to the slug. Ignored
   * for named sessions (whose display name is the name itself) and
   * for non-claude kinds.
   */
  claudeDisplayName?: string;
  /**
   * Resolved diff base ref for `{{base}}` substitution in
   * `config.diff.command`. Required by callers using a base-aware diff
   * command (the shipped default `hunk diff {{base}} --watch`); ignored
   * for non-diff kinds and for diff commands that don't reference
   * `{{base}}`. Substitution is verbatim — caller is responsible for
   * providing a ref that's safe to splice into a shell-quoted command
   * (the ref is double-quoted at spawn time).
   */
  base?: string;
}): Promise<AttachResult> {
  const { slug, cwd, kind, claudeName, claudeDisplayName, base } = opts;
  const name = sessionName(slug, kind, kind === "claude" ? claudeName : undefined);
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
      ? [
          "claude",
          ...wtSessionArgs({
            wtPath: cwd,
            name: claudeName,
            displayName:
              claudeName !== undefined
                ? claudeName
                : (claudeDisplayName ?? slug),
          }),
        ]
      : kind === "diff"
        ? [userShell, "-lc", resolveDiffCommand(config.diff.command, base)]
        : [userShell, "-l"];

  // Shell: pre-create the session detached and chain `pipe-pane` to a
  // per-slug log so every byte the shell writes is captured for the
  // bottom-pane tail (`core/shell-tail.ts`). Doing this before the
  // attach call (instead of chaining after `new-session -A`) closes
  // the would-be race where output written between session-create and
  // pipe-pane attach gets dropped — chained commands after `-A`
  // (attached) only run when the *client* detaches, far too late.
  // `-o` makes pipe-pane a no-op when already piping, so re-attaches
  // don't double-up. claude/diff don't need this — claude has its
  // own jsonl tail, and diff is a TUI we don't surface as an output.
  if (kind === "shell") {
    const shellLog = shellLogPath(slug);
    // pipe-pane runs its argument through /bin/sh -c, so the path
    // has to survive shell parsing — `homedir()` can contain spaces
    // (macOS "My Name" accounts) even though the slug can't.
    const quotedLog = shQuote(shellLog);
    const setup = Bun.spawn(
      [
        "tmux",
        "-L",
        TMUX_SOCKET,
        "-f",
        configPath,
        "new-session",
        "-A",
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
        "pipe-pane",
        "-o",
        "-t",
        name,
        // `>` not `>>` so a destroy-and-recreate of the same slug
        // doesn't seed the new tail with the prior session's lines.
        // pipe-pane spawns this shell once per session lifetime; the
        // truncate fires only on first attach, subsequent re-attaches
        // are `-o` no-ops and the existing FD keeps streaming.
        `cat > ${quotedLog}`,
      ],
      { stdout: "ignore", stderr: "pipe" },
    );
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
  if (stillRunning) return { kind: "detached" };
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

/**
 * Reconcile sessions against a live slug set. Kills any session of
 * any kind (claude, diff, or shell) whose underlying slug isn't in
 * `liveSlugs` — covers the case where a worktree was destroyed (in
 * this wt run or a prior one) without our session-kill hook firing.
 * The bare slug is derived by stripping the kind suffix so every
 * kind is reaped for a removed worktree. Errors are swallowed; an
 * orphaned session is a worse outcome than blocking startup.
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
