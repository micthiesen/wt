import type { HarnessId } from "../harness/index.ts";

/** Socket name (`-L`) for the wt-private tmux server. */
export const TMUX_SOCKET = "wt";

/**
 * Exit statuses used by the tmux client's `detach-client -E` bindings to
 * request an in-place jump to another worktree session. Keep these away from
 * ordinary command exit statuses so attach failures cannot be mistaken for a
 * navigation request.
 */
export const SESSION_SWITCH_EXIT_CODE = {
  shell: 110,
  diff: 111,
  harness: 112,
} as const;

export type SessionShortcut = keyof typeof SESSION_SWITCH_EXIT_CODE;

export function sessionSwitchTarget(
  code: number | null,
): SessionShortcut | null {
  for (const [target, switchCode] of Object.entries(
    SESSION_SWITCH_EXIT_CODE,
  ) as [SessionShortcut, number][]) {
    if (code === switchCode) return target;
  }
  return null;
}

/**
 * Slug for the persistent harness session at the wt source repo (the
 * `.` keybinding — one of two session slots, see
 * `tui/sessions/slots.ts`). Tmux session name is just `wt`; for the
 * claude harness the conversation also surfaces as `wt` in `/resume`
 * listings. The startup orphan reaper whitelists every slot slug so
 * this session survives the per-slug cleanup sweep.
 */
export const WT_SOURCE_SLUG = "wt";

/**
 * Reserved inner-server session for hub mode's right pane to idle on
 * when the selected task has no live session (see `core/hub.ts`). Runs
 * `wt _home` (a static dashboard). Not a worktree session: session
 * listing/classification and the orphan reaper must skip it, and it is
 * never user-attachable through the F-key or picker paths.
 */
export const HUB_HOME_SESSION = "wt-hub-home";

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
 * F-key codepath in this module — `core/tmux/action-sessions.ts` owns their
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

export const SUFFIX: Record<Exclude<SessionKind, "claude">, string> = {
  codex: "-codex",
  opencode: "-opencode",
  diff: "-diff",
  shell: "-shell",
  action: "-action",
};

export function harnessIdForKind(kind: SessionKind): HarnessId | null {
  if (kind === "claude" || kind === "codex" || kind === "opencode") return kind;
  return null;
}

/**
 * Separator between slug and a named claude session's user-supplied
 * name in the tmux session name. `~` was picked because it never
 * appears in slugs (derived from branch names — git accepts most
 * ASCII but `~` collides with reflog syntax in practice) and never in
 * validated session names (see `validateSessionName` in
 * `harness/claude/names.ts`). Stripping is unambiguous: rightmost `~`
 * splits slug from name.
 */
export const CLAUDE_NAMED_SEP = "~";

/**
 * Tmux session name for a (slug, kind, managedName). Claude has a
 * primary-vs-named distinction encoded in the name (`<slug>` vs
 * `<slug>~<name>`); every other kind uses a single fixed suffix
 * (`<slug>-<suffix>`). Codex / OpenCode are single-tmux-per-slug for
 * v1 — `managedName` is ignored for those.
 */
export function sessionName(
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
export function shQuote(s: string): string {
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
export function bareSlug(name: string): string {
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
