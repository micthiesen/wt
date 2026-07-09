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

// This module has been split into src/core/tmux/*.ts; this file is now
// a thin barrel reproducing the original export surface.

export { TMUX_SOCKET, WT_SOURCE_SLUG, claudeSessionName } from "./tmux/naming.ts";
export type { SessionKind } from "./tmux/naming.ts";

export { buildConfig, writeConfig } from "./tmux/config.ts";

export {
  killServer,
  killSession,
  killClaudeNamedSession,
  killHarnessSession,
  closeHarnessSessionGracefully,
  killDiffSession,
  killShellSession,
  killAllSessionsFor,
  diffCommandUsesBase,
  listSessions,
  reapOrphanedSessions,
} from "./tmux/admin.ts";
export type { ClaudeSessionEntry } from "./tmux/admin.ts";

export { attachOrCreate } from "./tmux/attach.ts";
export type { AttachResult } from "./tmux/attach.ts";

export { injectIntoSession } from "./tmux/inject.ts";
