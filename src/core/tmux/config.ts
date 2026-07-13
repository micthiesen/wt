import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

import { SESSION_SWITCH_EXIT_CODE } from "./naming.ts";

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
 *  - `unbind C-b` + F10/F11/F12 are context-aware. The key that owns
 *    the current session detaches back to wt; either other key exits
 *    the tmux client with a private status that asks the renderer-side
 *    navigator to attach the corresponding session immediately.
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
bind-key -n F10 if-shell -F '#{==:#{@wt-shortcut},shell}' 'detach-client' 'detach-client -E "exit ${SESSION_SWITCH_EXIT_CODE.shell}"'
bind-key -n F11 if-shell -F '#{==:#{@wt-shortcut},diff}' 'detach-client' 'detach-client -E "exit ${SESSION_SWITCH_EXIT_CODE.diff}"'
bind-key -n F12 if-shell -F '#{==:#{@wt-shortcut},harness}' 'detach-client' 'detach-client -E "exit ${SESSION_SWITCH_EXIT_CODE.harness}"'
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
