/**
 * Hub mode: an outer, chrome-only tmux server (`-L wt-hub`, one session
 * `hub`) that hosts wt's task-inbox TUI (`wt _taskpane`) alongside a
 * nested client into the existing inner `-L wt` server where all real
 * work (harness/diff/shell sessions) lives. See `hub/naming.ts` for the
 * pane-layout picture and `hub/config.ts` for why the outer server
 * mirrors the inner one's terminal-capability config.
 *
 * This module has been split into src/core/hub/*.ts; this file is a
 * thin barrel reproducing the intended export surface — mirroring
 * `core/tmux.ts`'s own barrel convention.
 */

export {
  HUB_FORWARD_KEYS,
  HUB_LEFT_PANE,
  HUB_RIGHT_PANE,
  HUB_SESSION,
  HUB_SOCKET,
  WT_HUB_ENV,
  WT_HUB_ENV_VALUE,
} from "./hub/naming.ts";

export { buildHubConfig, writeHubConfig } from "./hub/config.ts";

export { ensureHubLayout, launchHub, wtArgv } from "./hub/layout.ts";

export {
  ensureHomeSession,
  focusLeft,
  focusRight,
  isHubPane,
  killHub,
  resolveRightTty,
  showHome,
  switchRight,
} from "./hub/control.ts";
