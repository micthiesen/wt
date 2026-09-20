import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import { config } from "../config.ts";
import { SESSION_SWITCH_EXIT_CODE } from "./naming.ts";
import { readTerminalPaletteConfig } from "./palette.ts";

/** Path to the directory holding the generated `tmux.conf`. */
export function configDir(): string {
  const dir = config.paths.cacheRoot;
  mkdirSync(dir, { recursive: true });
  return dir;
}

/**
 * Terminal-capability preamble for the wt server's generated config.
 * Notable choices:
 *  - `status off` + `set-titles off`: no tmux chrome anywhere.
 *  - `alternate-screen on`: full-screen inner TUIs must be allowed to use
 *    smcup/rmcup. Disabling it leaves Codex in the normal scrollback buffer,
 *    so its initial viewport only grows as output arrives and composer
 *    redraws can leave the cursor visibly jumping around the screen.
 *  - `escape-time 0`: kills the 500ms ESC delay that breaks claude's
 *    keybindings.
 *  - `mouse on` + `focus-events on`: silences claude's "add this to
 *    your tmux.conf" advice and gives wheel-scroll + IDE focus.
 *  - Truecolor declared two ways (modern `terminal-features :RGB` +
 *    legacy `terminal-overrides :Tc`) — different tools check
 *    different paths.
 *  - `:sync` brackets physical client redraws in synchronized updates.
 *    Without it, a generic xterm-256color client such as Alacritty exposes
 *    intermediate cursor positions during Codex streaming and animations.
 *    This describes the outer terminal, separately from tmux accepting
 *    synchronized frames from the application inside its pane.
 *  - `extended-keys always` + `extended-keys-format csi-u` + `:extkeys`
 *    feature: lets tmux distinguish Shift+Enter from plain Enter so
 *    multiline shortcuts work through nested tmux/Codex/Claude sessions.
 *    The format option uses `-q`: tmux 3.4 lacks it and retains its native
 *    extended-key format instead of opening a configuration-error screen.
 *  - `:hyperlinks` preserves OSC 8 link boundaries through direct
 *    xterm-family clients, so the outer terminal does not have to
 *    guess where a URL ends.
 *  - `MouseDown1Pane` opens a stored OSC 8 destination directly. With
 *    tmux mouse mode on, terminals otherwise send the click to tmux instead
 *    of running their hyperlink action (Alacritty requires an extra Shift
 *    modifier in that state). Non-link clicks retain tmux's default
 *    select-pane + application-forwarding behavior.
 *  - Clipboard: `MouseDragEnd1Pane` pipes a completed selection to
 *    `pbcopy`, making drag-and-release match native macOS terminal copy
 *    behavior without enabling application-originated clipboard writes.
 *    `allow-passthrough on` lets desktop notifications + the progress bar
 *    reach the outer terminal instead of being swallowed by tmux. These
 *    mirror the user's global tmux config for modified-key forwarding.
 *  - `unbind C-b`: freed up for each config's own bindings below.
 */
export const TERMINAL_PREAMBLE = `set -g status off
set -g alternate-screen on
set -g set-titles off
set -sg escape-time 0
set -g mouse on
set -g focus-events on
set -g default-terminal "tmux-256color"
set -as terminal-features ",xterm*:RGB,tmux-256color:RGB"
set -as terminal-features ",xterm*:sync,tmux-256color:sync,alacritty*:sync"
set -ag terminal-overrides ",xterm-256color:Tc,tmux-256color:Tc"
set -ag update-environment "COLORTERM"
set -g allow-passthrough on
set -s extended-keys always
set -sq extended-keys-format csi-u
set -as terminal-features ",xterm*:extkeys,tmux-256color:extkeys"
set -as terminal-features ",xterm*:hyperlinks,tmux-256color:hyperlinks"
bind-key -n MouseDown1Pane if-shell -F '#{!=:#{mouse_hyperlink},}' 'run-shell -b "/usr/bin/open #{q:mouse_hyperlink}"' 'select-pane -t = \\; send-keys -M'
bind-key -T copy-mode MouseDragEnd1Pane send-keys -X copy-pipe-and-cancel pbcopy
bind-key -T copy-mode-vi MouseDragEnd1Pane send-keys -X copy-pipe-and-cancel pbcopy
unbind C-b`;

/**
 * Render the wt-private tmux config: the shared `TERMINAL_PREAMBLE`
 * plus this server's own bindings. `unbind C-b` + F10/F11/F12 are
 * context-aware. The key that owns the current session detaches back
 * to wt; either other key exits the tmux client with a private status
 * that asks the renderer-side navigator to attach the corresponding
 * session immediately.
 */
export function buildConfig(paletteConfig = "", terminalConfig = config.tmux.terminalConfig): string {
  return `${terminalConfig ?? TERMINAL_PREAMBLE}
${paletteConfig}
bind-key -n F10 if-shell -F '#{==:#{@wt-shortcut},shell}' 'detach-client' 'detach-client -E "exit ${SESSION_SWITCH_EXIT_CODE.shell}"'
bind-key -n F11 if-shell -F '#{==:#{@wt-shortcut},diff}' 'detach-client' 'detach-client -E "exit ${SESSION_SWITCH_EXIT_CODE.diff}"'
bind-key -n F12 if-shell -F '#{==:#{@wt-shortcut},harness}' 'detach-client' 'detach-client -E "exit ${SESSION_SWITCH_EXIT_CODE.harness}"'
`;
}

/**
 * Write `content` to `path` only if it differs from what's already
 * there. Callers use `changed` to decide whether to kill+restart the
 * tmux server (tmux only loads its config at server start).
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
  return writeIfChanged(path, buildConfig(readTerminalPaletteConfig(config.paths.cacheRoot)));
}

/**
 * Ensure a tmux config exists on disk WITHOUT the change-detection
 * kill-server dance, returning its path. For non-interactive codepaths
 * (`startHarnessSessionDetached`, and via it session messaging / the
 * `wt agent send` CLI) that may run from an arbitrary environment —
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
    writeFileSync(path, buildConfig(readTerminalPaletteConfig(config.paths.cacheRoot)), "utf8");
  }
  return path;
}
