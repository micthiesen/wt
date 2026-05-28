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
import { config } from "./config.ts";
import {
  getHarness,
  type Harness,
  type HarnessId,
} from "./harness/index.ts";
import { createLogger } from "./logger.ts";
import { shellLogPath } from "./shell-tail.ts";

const log = createLogger("[tmux]");

/** Socket name (`-L`) for the wt-private tmux server. */
export const TMUX_SOCKET = "wt";

/**
 * Slug for the persistent harness session at the wt source repo (the
 * `.` keybinding — one of two session slots, see
 * `tui/session-slots.ts`). Tmux session name is just `wt`; for the
 * claude harness the conversation also surfaces as `wt` in `/resume`
 * listings. The startup orphan reaper whitelists every slot slug so
 * this session survives the per-slug cleanup sweep.
 */
export const WT_SOURCE_SLUG = "wt";

/**
 * Kinds of session this module manages. `claude` / `codex` / `opencode`
 * are AI harness sessions (each spawned for one worktree at a time);
 * `diff` is the F11 git-diff TUI; `shell` is the F10 plain login
 * shell; `action` is a wt-managed action runner (claude `-p` or shell
 * command) supervised by tmux so it survives wt restarts.
 *
 * Each gets its own tmux session per slug. Naming:
 *  - `<slug>` for claude primary (back-compat)
 *  - `<slug>~<name>` for additional named claude sessions
 *  - `<slug>-codex` for the slug's codex tmux session (one at a time)
 *  - `<slug>-opencode` for the slug's opencode tmux session
 *  - `<slug>-diff` / `<slug>-shell` / `<slug>-action` for non-AI kinds
 *
 * Action sessions are not user-attachable and are not driven by the
 * F-key codepath in this module — `core/action-tmux.ts` owns their
 * lifecycle. The kind is registered here so `listSessions` and
 * `reapOrphanedSessions` see them uniformly with the other kinds.
 */
export type SessionKind =
  | "claude"
  | "codex"
  | "opencode"
  | "diff"
  | "shell"
  | "action";

const SUFFIX: Record<Exclude<SessionKind, "claude">, string> = {
  codex: "-codex",
  opencode: "-opencode",
  diff: "-diff",
  shell: "-shell",
  action: "-action",
};

function harnessIdForKind(kind: SessionKind): HarnessId | null {
  if (kind === "claude" || kind === "codex" || kind === "opencode") return kind;
  return null;
}

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

/**
 * Tmux session name for a (slug, kind, managedName). Claude has a
 * primary-vs-named distinction encoded in the name (`<slug>` vs
 * `<slug>~<name>`); every other kind uses a single fixed suffix
 * (`<slug>-<suffix>`). Codex / OpenCode are single-tmux-per-slug for
 * v1 — `managedName` is ignored for those.
 */
function sessionName(
  slug: string,
  kind: SessionKind,
  managedName: string | null = null,
): string {
  if (kind === "claude") return claudeSessionName(slug, managedName);
  return `${slug}${SUFFIX[kind]}`;
}

/**
 * Tmux session name for a claude session. Primary (`name = null`) is
 * the bare slug; named is `<slug>~<name>`. Single source of truth so
 * every consumer (tmux composer, session-tail key, activity-pane log
 * line) agrees on the format.
 */
export function claudeSessionName(slug: string, name: string | null): string {
  return name === null ? slug : `${slug}${CLAUDE_NAMED_SEP}${name}`;
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
 * Recover the bare slug from a tmux session name.
 *
 * A `~` in the name unambiguously marks a named claude session: the
 * slug is everything before the rightmost `~`. The kind suffix
 * (`-codex` / `-opencode` / `-diff` / `-shell` / `-action`) is only
 * a possibility for names that have NO `~`, since named claudes are
 * claude-only and never carry a kind suffix.
 *
 * Pre-existing collision: a slug ending in any of the kind suffixes
 * (a description like "Add codex" or a branch like `eng-1234-codex`
 * slugifies into one) makes its primary claude session
 * indistinguishable from a same-namespace `<bare>-codex` session in
 * tmux. `-codex` and `-opencode` make this materially riskier than
 * the old `-diff`/`-shell` collisions because AI harness names are
 * plausible branch-description words. The only proper fixes are
 * slug-level validation or moving kinds to a separator that can't
 * appear in slugs. Out of scope here; flagged for a future sweep.
 */
function bareSlug(name: string): string {
  // Strip a trailing `~<name>` if present (claude named, or any
  // future per-id-named harness session).
  const tildeIdx = name.lastIndexOf(CLAUDE_NAMED_SEP);
  const beforeTilde = tildeIdx >= 0 ? name.slice(0, tildeIdx) : name;
  // Strip the kind suffix (`-codex`, `-opencode`, `-diff`, `-shell`,
  // `-action`) from what's left. Order matters: longest-suffix first
  // so `-opencode` doesn't get partially-stripped by `-code` if a
  // future entry were added.
  const suffixes = Object.values(SUFFIX).sort((a, b) => b.length - a.length);
  for (const suffix of suffixes) {
    if (beforeTilde.endsWith(suffix)) return beforeTilde.slice(0, -suffix.length);
  }
  return beforeTilde;
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

/**
 * Kill one worktree's harness session by id. For Claude with a non-null
 * managedName, kills the named session; otherwise kills the primary
 * tmux slot for that harness on that slug. Idempotent.
 */
export async function killHarnessSession(
  slug: string,
  harnessId: HarnessId,
  managedName: string | null = null,
): Promise<void> {
  await killByName(sessionName(slug, harnessId, managedName));
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
  // hardcoding the named-claude list. The action kill goes via
  // action-tmux.ts (now async like the rest).
  const all = await listAllSessionsRaw().catch(() => new Set<string>());
  const ours = [...all].filter((n) => bareSlug(n) === slug);
  await Promise.allSettled([
    ...ours.map((n) => killByName(n)),
    killActionSession(slug),
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
    { stdout: "ignore", stderr: "pipe" },
  );
  const [code, errText] = await Promise.all([
    proc.exited,
    new Response(proc.stderr).text(),
  ]);
  // tmux exits non-zero for "session not found" (the desired no-op
  // path when killing an absent slot) but ALSO for connection /
  // permission failures. Filter the benign case so the noise floor
  // is low, but surface real errors so a failed kill doesn't look
  // like silent success.
  if (code !== 0 && !/can't find session/i.test(errText)) {
    log.warn("tmux kill-session failed", {
      name,
      code,
      stderr: errText.trim() || null,
    });
  }
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
 * `claude` is a list of `(slug, name)` because a single worktree can
 * host multiple claude sessions (primary + N named). `codex` and
 * `opencode` are slug sets — for v1 they're single-tmux-per-slug.
 * The legacy `claudeSlugs` set is the unique-slug projection of
 * `claude` — preserved so "row has any live claude" checks stay a
 * Set lookup.
 *
 * Server-not-running exits non-zero with a "no server running"
 * stderr; we map that to empty sets rather than throwing — it's the
 * steady state when no worktree has been entered yet.
 */
export async function listSessions(): Promise<{
  claude: ClaudeSessionEntry[];
  claudeSlugs: Set<string>;
  codex: Set<string>;
  opencode: Set<string>;
  diff: Set<string>;
  shell: Set<string>;
  action: Set<string>;
  /** Raw set of every live tmux session name. Used by harness impls
   *  to compute `isLive` without a second `list-sessions` call. */
  all: Set<string>;
}> {
  const all = await listAllSessionsRaw();
  const claude: ClaudeSessionEntry[] = [];
  const claudeSlugs = new Set<string>();
  const codex = new Set<string>();
  const opencode = new Set<string>();
  const diff = new Set<string>();
  const shell = new Set<string>();
  const action = new Set<string>();
  for (const name of all) {
    if (name.endsWith(SUFFIX.codex)) {
      codex.add(name.slice(0, -SUFFIX.codex.length));
    } else if (name.endsWith(SUFFIX.opencode)) {
      opencode.add(name.slice(0, -SUFFIX.opencode.length));
    } else if (name.endsWith(SUFFIX.diff)) {
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
  return { claude, claudeSlugs, codex, opencode, diff, shell, action, all };
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
   * command (the shipped default `hunk diff {{base}} --watch`); ignored
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
    const setup = Bun.spawn(setupArgs, { stdout: "ignore", stderr: "pipe" });
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

const sleep = (ms: number): Promise<void> =>
  new Promise((resolve) => setTimeout(resolve, ms));

/** Named tmux paste buffer used by `injectIntoSession`. */
const INJECT_BUFFER = "wt-inject";
/** Settle pause when the session is already running before pasting. */
const WARM_SETTLE_MS = 300;
/** capture-pane poll interval while waiting for a cold start to render. */
const READY_POLL_MS = 350;
/** Hard cap on the cold-start readiness wait; inject anyway after this. */
const READY_MAX_MS = 12_000;
/** Gap between the paste landing and the Enter that submits it. */
const SUBMIT_DELAY_MS = 500;

/**
 * Exact-match target for the *pane* commands below (capture-pane,
 * paste-buffer, send-keys). Their `-t` is a target-pane, where the bare
 * `=name` exact-session prefix that works for `kill-session` is rejected
 * with "can't find pane". The form that both targets the session's active
 * pane AND keeps exact (non-prefix) matching is `=<name>:` — the trailing
 * colon selects the session's current window. Don't drop the colon.
 */
function paneTarget(name: string): string {
  return `=${name}:`;
}

/** Run a tmux command on our private server; collect exit code + stderr. */
async function runTmux(
  args: readonly string[],
): Promise<{ code: number; stderr: string }> {
  const proc = Bun.spawn(["tmux", "-L", TMUX_SOCKET, ...args], {
    stdout: "ignore",
    stderr: "pipe",
  });
  const [code, stderr] = await Promise.all([
    proc.exited,
    new Response(proc.stderr).text(),
  ]);
  return { code, stderr };
}

/** Snapshot a session's active pane as plain text, or null on failure. */
async function capturePane(name: string): Promise<string | null> {
  const proc = Bun.spawn(
    ["tmux", "-L", TMUX_SOCKET, "capture-pane", "-p", "-t", paneTarget(name)],
    { stdout: "pipe", stderr: "ignore" },
  );
  const [out, code] = await Promise.all([
    new Response(proc.stdout).text(),
    proc.exited,
  ]);
  return code === 0 ? out : null;
}

/**
 * Wait until a freshly-started claude pane stops changing — meaning it
 * has finished its initial render and is sitting at an idle prompt — or
 * the cap elapses. Stability (two identical, non-trivial snapshots) is
 * version-agnostic: we never scrape claude's exact prompt string, we
 * just watch for the screen to settle. A startup spinner keeps the pane
 * changing, so the loop naturally waits out a slow boot instead of
 * guessing a fixed delay. Returns whether it settled (false = hit the
 * cap; the caller pastes anyway).
 */
async function waitForPaneReady(name: string): Promise<boolean> {
  const deadline = Date.now() + READY_MAX_MS;
  let prev: string | null = null;
  // Initial grace — claude writes nothing for the first beat after spawn.
  await sleep(READY_POLL_MS);
  while (Date.now() < deadline) {
    const cur = (await capturePane(name))?.trim() ?? "";
    if (cur.length > 0 && cur === prev) return true;
    prev = cur;
    await sleep(READY_POLL_MS);
  }
  return false;
}

/**
 * Create the worktree's primary claude session detached (no client
 * attach). Byte-for-byte the session `attachOrCreate({kind:"claude"})`
 * would make — same name (`<slug>`), same `buildArgs` argv, same stderr
 * wrapper and TMUX-stripping env — so a later F12 `new-session -A` just
 * attaches to this one rather than spawning a second. Sized generously
 * so claude doesn't render cramped before the user attaches; tmux
 * resizes to the client on attach.
 */
async function startClaudeSessionDetached(
  slug: string,
  cwd: string,
): Promise<{ ok: boolean; reason?: string }> {
  const name = sessionName(slug, "claude");
  const { path: configPath, changed } = writeConfig();
  if (changed) {
    log.info("config changed, killing server before detached claude start", {
      slug,
    });
    await killServer();
  }
  const stderrPath = join(sessionsDir(), `${name}.err`);
  const innerArgs = getHarness("claude").buildArgs({
    wtPath: cwd,
    managedName: null,
    resumeSessionId: null,
  });
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
        "-d",
        "-s",
        name,
        "-c",
        cwd,
        "-x",
        "200",
        "-y",
        "50",
        // See attachOrCreate header: claude downgrades to 256-color when
        // $TMUX is set, so strip it before exec'ing. The bash wrapper
        // redirects stderr to a file so a spawn-and-die surfaces a reason.
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
      ],
      {
        cwd,
        stdout: "ignore",
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
    log.error("detached claude start spawn failed", { slug, reason });
    return { ok: false, reason };
  }
  const [code, stderr] = await Promise.all([
    proc.exited,
    new Response(proc.stderr as ReadableStream<Uint8Array>).text(),
  ]);
  if (code !== 0) {
    const reason = stderr.trim() || `tmux new-session exited ${code}`;
    log.warn("detached claude start failed", { slug, code, reason });
    return { ok: false, reason };
  }
  return { ok: true };
}

/** Pipe text into the inject buffer and paste it into a session's pane. */
async function pasteBuffer(name: string, text: string): Promise<void> {
  // load-buffer reads stdin, so arbitrary text (quotes, `$`, newlines)
  // needs no shell escaping.
  const load = Bun.spawn(
    ["tmux", "-L", TMUX_SOCKET, "load-buffer", "-b", INJECT_BUFFER, "-"],
    {
      stdin: new TextEncoder().encode(text),
      stdout: "ignore",
      stderr: "ignore",
    },
  );
  await load.exited;
  // `-p` = bracketed paste (claude receives it as one chunk, so internal
  // newlines and a leading slash command don't submit early); `-d` drops
  // the buffer after.
  await runTmux([
    "paste-buffer",
    "-d",
    "-p",
    "-b",
    INJECT_BUFFER,
    "-t",
    paneTarget(name),
  ]);
}

/**
 * Send `text` to a worktree's primary (F12) claude session as if typed
 * at the prompt, then submit it. Starts the session first if it isn't
 * running, waiting for it to finish booting before pasting. The prompt
 * lands in the live conversation — with its existing context and history
 * — rather than a fresh headless `claude -p` run.
 *
 * Fire-and-forget by nature: there's no completion sentinel the way a
 * `claude -p` action has, so callers can't observe when claude finishes.
 *
 * Known edge: a brand-new worktree directory claude has never run in may
 * show its trust prompt on cold start; the paste+Enter would answer that
 * dialog instead of submitting. Attaching via F12 once (to accept trust)
 * before injecting avoids it.
 */
export async function injectIntoSession(opts: {
  slug: string;
  cwd: string;
  text: string;
}): Promise<{ ok: true; coldStarted: boolean } | { ok: false; reason: string }> {
  const { slug, cwd, text } = opts;
  const name = sessionName(slug, "claude");
  const running = (
    await listAllSessionsRaw().catch(() => new Set<string>())
  ).has(name);
  let coldStarted = false;
  if (!running) {
    const started = await startClaudeSessionDetached(slug, cwd);
    if (!started.ok) {
      return {
        ok: false,
        reason: started.reason ?? "failed to start claude session",
      };
    }
    coldStarted = true;
    await waitForPaneReady(name);
  } else {
    await sleep(WARM_SETTLE_MS);
  }
  try {
    await pasteBuffer(name, text);
    await sleep(SUBMIT_DELAY_MS);
    const { code, stderr } = await runTmux([
      "send-keys",
      "-t",
      paneTarget(name),
      "Enter",
    ]);
    if (code !== 0) {
      return {
        ok: false,
        reason: stderr.trim() || `tmux send-keys exited ${code}`,
      };
    }
  } catch (err) {
    return {
      ok: false,
      reason: err instanceof Error ? err.message : String(err),
    };
  }
  return { ok: true, coldStarted };
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
