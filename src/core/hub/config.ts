/**
 * Render + write the outer hub server's tmux.conf.
 *
 * The base terminal-capability lines mirror `core/tmux/config.ts`
 * (`buildConfig`) exactly — the outer server's left pane runs `wt
 * _taskpane` and the right pane's client passes through into the inner
 * server, so both need the same truecolor / extended-keys / no-flash
 * settings the inner config exists for. What's different here is the
 * *purpose* of the key table: the inner config's F10/F11/F12 bindings
 * are context-aware session-switch requests owned by whichever session
 * is active; this config's bindings are a dumb relay — every Alt-<key>
 * chord on the root table gets forwarded verbatim into the left pane
 * via `send-keys`, because the user's terminal focus normally sits on
 * the right (harness) pane and wt still needs a way to receive input.
 *
 * Two bindings are handled by tmux itself rather than forwarded:
 *   - `F8` zooms the right pane (a full-screen harness view).
 *   - `F9` is forwarded to the task pane like F10-F12: wt toggles the
 *     focus itself (select-pane) so its focus indicator can never drift
 *     from reality — terminal focus events stay a mouse-click fallback.
 *
 * Pane resilience: `remain-on-exit` + a `pane-died` hook means a
 * crashed left `wt _taskpane` or a crashed right tmux client
 * auto-respawns instead of leaving a dead pane — harness sessions on
 * the INNER server are untouched either way, since this server never
 * hosts them.
 *
 * Every forwarded binding (including `M-;`) is wrapped in tmux's
 * `{ ... }` command-group syntax rather than a bare `send-keys ...`
 * command line. Bare `bind -n 'M-;' send-keys -t hub:0.0 ';'` parses
 * fine but tmux drops the trailing `;` argument at config-parse time
 * regardless of quoting — `;` is tmux's own command separator, and
 * outside a `{ }` group it terminates the `send-keys` command instead
 * of being passed through as its final argument. Wrapping every
 * generated bind in `{ }` (not just the `;` one) keeps the bindings
 * uniform rather than special-casing the one key that needs it.
 * Live-verified on a throwaway `-L fixprobe` socket: `list-keys -T
 * root` showed `send-keys -t hub:0.0` with the `;` argument silently
 * missing under the bare form, and present under the `{ }` form.
 */
import { join } from "node:path";

import { configDir, TERMINAL_PREAMBLE, writeIfChanged } from "../tmux/config.ts";
import { HUB_FORWARD_KEYS, HUB_LEFT_PANE, HUB_RIGHT_PANE } from "./naming.ts";

/**
 * Quote a single tmux config-file token when it contains a character
 * that's actually special to tmux's parser: `;` (command separator),
 * `'`/`"` (quoting), `#` (comment), or whitespace. Everything else
 * (letters, digits, and punctuation like `!` `[` `]` `,` `.` `/` `>`
 * `?` that has no meaning to tmux) is left bare — over-quoting every
 * bind would work too, but the bare form is what a human editing this
 * file by hand would write, so tests (and any future diffing against a
 * hand-written config) stay readable. Single-quote by default; switch
 * to double-quotes when the token itself contains a single quote (`'`)
 * — UNLESS it also contains a double quote, in which case neither bare
 * delimiter is safe and the token's internal `"` characters are
 * backslash-escaped so it can still be wrapped in `"..."`.
 */
export function tmuxQuote(token: string): string {
  if (!/[;"'#\s]/.test(token)) return token;
  if (!token.includes("'")) return `'${token}'`;
  if (!token.includes('"')) return `"${token}"`;
  return `"${token.replace(/"/g, '\\"')}"`;
}

export function buildHubConfig(): string {
  const forwardLines = HUB_FORWARD_KEYS.map((key) => {
    const bindKey = tmuxQuote(`M-${key}`);
    const arg = tmuxQuote(key);
    return `bind -n ${bindKey} { send-keys -t ${HUB_LEFT_PANE} ${arg} }`;
  }).join("\n");

  return `${TERMINAL_PREAMBLE}
set -g remain-on-exit on
set-hook -g pane-died respawn-pane
# tmux cannot remove the divider column between panes, only recolor it.
# Painting the border glyphs in the terminal's background (Alacritty
# Catppuccin Mocha base, which applyHubPalette also uses as the task
# pane's bg — keep the three in sync) makes the bar visually disappear;
# focus is signaled inside the task pane, so active/inactive match.
set -g pane-border-style "fg=#1E1E2E"
set -g pane-active-border-style "fg=#1E1E2E"
${forwardLines}
bind -n M-Enter { send-keys -t ${HUB_LEFT_PANE} Enter }
bind -n M-Tab { send-keys -t ${HUB_LEFT_PANE} Tab }
bind -n F10 { send-keys -t ${HUB_LEFT_PANE} F10 }
bind -n F11 { send-keys -t ${HUB_LEFT_PANE} F11 }
bind -n F12 { send-keys -t ${HUB_LEFT_PANE} F12 }
bind -n F8 { resize-pane -Z -t ${HUB_RIGHT_PANE} }
bind -n F9 { send-keys -t ${HUB_LEFT_PANE} F9 }
`;
}

/**
 * Write the hub config to disk if it differs from what's already
 * there. Delegates to `core/tmux/config.ts`'s shared `writeIfChanged`
 * — callers use `changed` to decide whether to kill+restart the hub
 * server, since tmux only loads its config at server start.
 */
export function writeHubConfig(): { path: string; changed: boolean } {
  const path = join(configDir(), "hub-tmux.conf");
  return writeIfChanged(path, buildHubConfig());
}
