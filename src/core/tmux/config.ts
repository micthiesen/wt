import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

import { SESSION_SWITCH_EXIT_CODE } from "./naming.ts";

/**
 * Path to the directory holding every generated tmux config (this
 * server's `tmux.conf` and hub mode's `hub-tmux.conf`) — shared so
 * `core/hub/config.ts` doesn't duplicate the `~/.cache/wt` join +
 * mkdir dance.
 */
export function configDir(): string {
  const dir = join(homedir(), ".cache", "wt");
  mkdirSync(dir, { recursive: true });
  return dir;
}

/**
 * Terminal-capability preamble shared verbatim by this server's config
 * and hub mode's outer-server config (`core/hub/config.ts`'s
 * `buildHubConfig`) — the hub's left pane runs `wt _taskpane` and its
 * right pane's client passes through into THIS server, so both need
 * identical truecolor / extended-keys / no-flash settings for the
 * passthrough to be transparent. Notable choices:
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
 *  - `extended-keys always` + `extended-keys-format csi-u` + `:extkeys`
 *    feature: lets tmux distinguish Shift+Enter from plain Enter so
 *    multiline shortcuts work through nested tmux/Codex/Claude sessions.
 *    `allow-passthrough on` lets desktop notifications + the progress bar
 *    reach the outer terminal instead of being swallowed by tmux. These
 *    mirror the user's global tmux config for modified-key forwarding.
 *  - `unbind C-b`: freed up for each config's own bindings below.
 */
export const TERMINAL_PREAMBLE = `set -g status off
set -g alternate-screen off
set -g set-titles off
set -sg escape-time 0
set -g mouse on
set -g focus-events on
set -g default-terminal "tmux-256color"
set -as terminal-features ",xterm*:RGB,tmux-256color:RGB"
set -ag terminal-overrides ",xterm-256color:Tc,tmux-256color:Tc"
set -ag update-environment "COLORTERM"
set -g allow-passthrough on
set -s extended-keys always
set -s extended-keys-format csi-u
set -as terminal-features ",xterm*:extkeys,tmux-256color:extkeys"
unbind C-b`;

/**
 * Render the wt-private tmux config: the shared `TERMINAL_PREAMBLE`
 * plus this server's own bindings. `unbind C-b` + F10/F11/F12 are
 * context-aware. The key that owns the current session detaches back
 * to wt; either other key exits the tmux client with a private status
 * that asks the renderer-side navigator to attach the corresponding
 * session immediately.
 */
export function buildConfig(): string {
  return `${TERMINAL_PREAMBLE}
bind-key -n F10 if-shell -F '#{==:#{@wt-shortcut},shell}' 'detach-client' 'detach-client -E "exit ${SESSION_SWITCH_EXIT_CODE.shell}"'
bind-key -n F11 if-shell -F '#{==:#{@wt-shortcut},diff}' 'detach-client' 'detach-client -E "exit ${SESSION_SWITCH_EXIT_CODE.diff}"'
bind-key -n F12 if-shell -F '#{==:#{@wt-shortcut},harness}' 'detach-client' 'detach-client -E "exit ${SESSION_SWITCH_EXIT_CODE.harness}"'
`;
}

/**
 * Write `content` to `path` only if it differs from what's already
 * there. Shared by this module's `writeConfig` and hub mode's
 * `writeHubConfig` — both need the same read-prev/compare/write-if-
 * changed shape, since callers on both sides use `changed` to decide
 * whether to kill+restart the affected tmux server (tmux only loads
 * its config at server start).
 */
export function writeIfChanged(path: string, content: string): { path: string; changed: boolean } {
  let prev = "";
  try {
    prev = readFileSync(path, "utf8");
  } catch {
    // first run
  }
  const changed = prev !== content;
  if (changed) writeFileSync(path, content, "utf8");
  return { path, changed };
}

/**
 * Write the config to disk if it differs from what's already there.
 * Returns the path and whether the file actually changed — callers use
 * `changed` to decide whether to kill+restart the server (see header).
 */
export function writeConfig(): { path: string; changed: boolean } {
  const path = join(configDir(), "tmux.conf");
  return writeIfChanged(path, buildConfig());
}

/**
 * Ensure a tmux config exists on disk WITHOUT the change-detection
 * kill-server dance, returning its path. For non-interactive codepaths
 * (`startHarnessSessionDetached`, and via it `injectIntoSession` / the
 * `wt claude send` CLI) that may run from an arbitrary environment —
 * including from a claude session INSIDE the wt tmux server itself,
 * where TERM is `tmux-256color` rather than the user's outer terminal.
 * There `buildConfig()` renders differently than what the user's wt
 * wrote, so the `writeConfig()` + `killServer()` path would (a) poison
 * the on-disk config with the wrong terminal capabilities and (b) kill
 * every live session, including the very session that invoked the CLI.
 * The config only matters at server start anyway; an already-running
 * server ignores `-f` entirely.
 */
export function ensureConfig(): string {
  const path = join(configDir(), "tmux.conf");
  try {
    readFileSync(path, "utf8");
  } catch {
    writeFileSync(path, buildConfig(), "utf8");
  }
  return path;
}
