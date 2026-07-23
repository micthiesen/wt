/**
 * Shared constants for hub mode.
 *
 * Hub mode runs an OUTER tmux server (socket `wt-hub`, one session
 * named `hub`) that just holds a two-pane layout:
 *   - `hub:0.0` (left, ~35 cols) — `wt _taskpane`, the task-inbox TUI.
 *   - `hub:0.1` (right) — a nested tmux CLIENT (with `TMUX` unset) into
 *     the EXISTING inner `-L wt` server where every harness/diff/shell
 *     session actually lives.
 *
 * The outer server never hosts real work — it's pure chrome around the
 * inner one, which is why it's cheap to kill and rebuild on config
 * changes (see `launchHub` in `hub/layout.ts`). Constants live in their
 * own file (mirroring `core/tmux/naming.ts`) so `hub/config.ts`,
 * `hub/layout.ts`, and `hub/control.ts` can all depend on them without
 * a cycle.
 */

/** Socket name (`-L`) for the outer, hub-only tmux server. */
export const HUB_SOCKET = "wt-hub";

/** The (single) session name on the hub socket. */
export const HUB_SESSION = "hub";

/** Left pane target: `wt _taskpane`, the task-inbox TUI. */
export const HUB_LEFT_PANE = `${HUB_SESSION}:0.0`;

/** Right pane target: the nested client into the inner `-L wt` server. */
export const HUB_RIGHT_PANE = `${HUB_SESSION}:0.1`;

/**
 * Env var name set (to `WT_HUB_ENV_VALUE`) on the left pane's `wt`
 * process so it can tell at runtime that it's running as the
 * task-inbox half of hub mode rather than a normal interactive `wt`.
 * See `isHubPane` in `hub/control.ts`.
 */
export const WT_HUB_ENV = "WT_HUB";

/** Value `WT_HUB_ENV` is set to; anything else (including unset) is "not hub". */
export const WT_HUB_ENV_VALUE = "1";

/**
 * Keys forwarded from the outer server's root key table into the left
 * pane via `Alt-<key> -> send-keys hub:0.0 <key>`.
 *
 * The user's terminal focus sits on the right pane (harness input), so
 * wt is driven through Alt-prefixed chords that the outer server's root
 * table intercepts and relays into the left `wt _taskpane` process as
 * the bare key — from `_taskpane`'s point of view it's an ordinary
 * (unmodified) keypress. Letters include both cases: an uppercase
 * entry (`G`, `N`, `J`, …) is a *separate* forward for Alt+Shift+<key>,
 * since tmux's key table distinguishes `M-g` from `M-G` the same way it
 * distinguishes `g` from `G`.
 *
 * The primary driver of this table is the CMD LAYER: Alacritty maps
 * cmd+<key> to the same ESC-prefixed sequences (skhd/yabai own most of
 * the Alt space globally, so bare Option chords for j/k/n/1-5 never
 * reach the terminal). `h`/`d`/`s`/`f`/`w` are deliberately absent
 * here — they have dedicated rebinds in `buildHubConfig` (F7 focus /
 * F11 diff / F10 shell / zoom / C-d close) instead of forwarding the
 * literal letter, as does `t` (cmd+t = new worktree, rebound to `n`;
 * cmd+n stays Alacritty's new-window). Digits 1-9 forward for the
 * task quick-jump.
 */
export const HUB_FORWARD_KEYS = [
  "j",
  "k",
  "1",
  "2",
  "3",
  "4",
  "5",
  "6",
  "7",
  "8",
  "9",
  "g",
  "G",
  "n",
  "N",
  "c",
  "a",
  "i",
  "y",
  "r",
  "p",
  "e",
  "E",
  "m",
  "v",
  "l",
  "L",
  "J",
  "K",
  "b",
  "R",
  "A",
  "P",
  "z",
  "I",
  "q",
  "o",
  "O",
  ";",
  "!",
  "'",
  "[",
  "]",
  ">",
  "?",
] as const;
