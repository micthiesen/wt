/**
 * Hub mode's control surface: the handful of operations wt itself
 * (running as the left `_taskpane` pane) uses to steer the layout —
 * telling whether it's running inside hub mode at all, retargeting the
 * right pane at a different inner-server session, focusing a pane, and
 * tearing the hub down. Everything here is best-effort and
 * non-throwing; a failed tmux call degrades to "nothing visibly
 * happened" rather than crashing the task-inbox TUI.
 */
import { homedir } from "node:os";

import { createLogger } from "../logger.ts";
import { ensureConfig } from "../tmux/config.ts";
import { HUB_HOME_SESSION } from "../tmux/naming.ts";
import { TMUX_SOCKET } from "../tmux.ts";
import { wtArgv } from "./layout.ts";
import { HUB_LEFT_PANE, HUB_RIGHT_PANE, HUB_SESSION, HUB_SOCKET, WT_HUB_ENV, WT_HUB_ENV_VALUE } from "./naming.ts";
import { spawnTmux } from "./proc.ts";

const log = createLogger("[hub]");

/** True when this process is the left `wt _taskpane` pane of a running hub. */
export function isHubPane(): boolean {
  return process.env[WT_HUB_ENV] === WT_HUB_ENV_VALUE;
}

/**
 * Module-level cache for the right pane's tty path. Resolving it is a
 * tmux round-trip; every retarget (`switchRight`) needs it, and it only
 * changes if the hub layout itself gets rebuilt (`launchHub` config
 * change, or a respawned pane after a crash) — both rare compared to
 * how often retargets happen. `invalidateRightTty` is the escape hatch
 * for exactly those cases.
 */
let rightTtyCache: string | null = null;

/**
 * Drop the cached right-pane tty so the next `resolveRightTty`
 * re-queries. Not exported beyond this module — the only caller is
 * `switchRight`'s own retry path below, when a switch-client fails and
 * the pane may have been respawned since the cache was filled.
 */
function invalidateRightTty(): void {
  rightTtyCache = null;
}

/** The right pane's tty path (`#{pane_tty}`), or null if it can't be resolved. */
export async function resolveRightTty(): Promise<string | null> {
  if (rightTtyCache) return rightTtyCache;
  const r = await spawnTmux(HUB_SOCKET, [
    "display-message",
    "-p",
    "-t",
    HUB_RIGHT_PANE,
    "#{pane_tty}",
  ]);
  if (r.code !== 0) {
    log.warn("resolveRightTty: display-message failed", { stderr: r.stderr.trim() || null });
    return null;
  }
  const tty = r.stdout.trim();
  if (!tty) return null;
  rightTtyCache = tty;
  return tty;
}

/** How many `switch-client` attempts `switchRight` makes against a single tty before giving up on it. */
const SWITCH_COLD_START_ATTEMPTS = 3;

/** Delay between cold-start retry attempts (see `switchRight`). */
const SWITCH_COLD_START_DELAY_MS = 300;

function sleep(ms: number): Promise<void> {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * tmux's `-t` target syntax parses `:` and `.` inside the name as the
 * window/pane separators, so a bare session name containing either
 * (real for branch-derived slugs like a worktree on `release/v1.2`,
 * which the inner server's sessions are named after) gets silently
 * mis-parsed into a different target than intended. Prefixing with
 * `=` switches tmux into exact-match mode, where the rest of the
 * string is taken as a literal session name. Session-target `-t` args
 * need this; `-s` (session name, not a target expression) does not.
 */
function exactSessionTarget(sessionName: string): string {
  return `=${sessionName}`;
}

/**
 * Retarget the right pane's tty at a different session on the INNER
 * `-L wt` server — this is how wt "opens" a worktree's harness/diff/
 * shell session into the pane the user is actually looking at.
 *
 * Two independent failure modes get their own handling:
 *  - Cold start: right after `ensureHubLayout` splits the right pane,
 *    the nested tmux client `rightPaneCommand` spawns into it hasn't
 *    necessarily finished attaching to the inner server by the time
 *    the first `switchRight` call lands, so `switch-client -c <tty>`
 *    can fail with "client not found" for a brief window. The first
 *    attempt is retried up to `SWITCH_COLD_START_ATTEMPTS` times with
 *    a `SWITCH_COLD_START_DELAY_MS` delay between attempts to ride out
 *    that window.
 *  - Stale tty: the cached tty itself may be wrong (the pane was
 *    respawned since the cache was filled) — invalidated and
 *    re-resolved once, then given a single further attempt (not the
 *    cold-start retry budget again, to keep the worst case bounded).
 * Worst case is `SWITCH_COLD_START_ATTEMPTS * SWITCH_COLD_START_DELAY_MS`
 * (~900ms with current constants) plus one extra attempt, i.e. roughly
 * the ~1s the constants were picked to stay under. Debouncing rapid
 * retargets (e.g. fast row navigation) is the caller's responsibility.
 */
export async function switchRight(sessionName: string): Promise<boolean> {
  const tty = await resolveRightTty();
  if (!tty) {
    log.warn("switchRight: no right pane tty available", { sessionName });
    return false;
  }
  const target = exactSessionTarget(sessionName);
  let r = await switchClientWithColdStartRetries(tty, target);
  if (r.code === 0) return true;

  invalidateRightTty();
  const retryTty = await resolveRightTty();
  if (!retryTty) {
    log.warn("switchRight: failed and tty re-resolve came back empty", {
      sessionName,
      stderr: r.stderr.trim() || null,
    });
    return false;
  }
  r = await spawnTmux(TMUX_SOCKET, ["switch-client", "-c", retryTty, "-t", target]);
  if (r.code !== 0) {
    log.warn("switchRight: switch-client failed after retry", {
      sessionName,
      stderr: r.stderr.trim() || null,
    });
    return false;
  }
  return true;
}

/** `switch-client -c tty -t target`, retried through the cold-start window (see `switchRight`). */
async function switchClientWithColdStartRetries(
  tty: string,
  target: string,
): Promise<{ code: number; stdout: string; stderr: string }> {
  let r = await spawnTmux(TMUX_SOCKET, ["switch-client", "-c", tty, "-t", target]);
  for (let attempt = 1; r.code !== 0 && attempt < SWITCH_COLD_START_ATTEMPTS; attempt++) {
    await sleep(SWITCH_COLD_START_DELAY_MS);
    r = await spawnTmux(TMUX_SOCKET, ["switch-client", "-c", tty, "-t", target]);
  }
  return r;
}

/**
 * Ensure the reserved `wt-hub-home` session exists on the inner
 * server (detached — nothing attaches to it directly; `showHome`
 * retargets the right pane at it via `switchRight`). No-op when it's
 * already there.
 */
export async function ensureHomeSession(): Promise<void> {
  const has = await spawnTmux(TMUX_SOCKET, [
    "has-session",
    "-t",
    exactSessionTarget(HUB_HOME_SESSION),
  ]);
  if (has.code === 0) return;
  const configPath = ensureConfig();
  const r = await spawnTmux(TMUX_SOCKET, [
    "-f",
    configPath,
    "new-session",
    "-d",
    "-s",
    HUB_HOME_SESSION,
    "-c",
    homedir(),
    ...wtArgv(),
    "_home",
  ]);
  if (r.code !== 0) {
    log.warn("ensureHomeSession: new-session failed", { stderr: r.stderr.trim() || null });
  }
}

/** Ensure the home session exists, then park the right pane on it. */
export async function showHome(): Promise<boolean> {
  await ensureHomeSession();
  return switchRight(HUB_HOME_SESSION);
}

/** Focus the left (`wt _taskpane`) pane. */
export async function focusLeft(): Promise<void> {
  await spawnTmux(HUB_SOCKET, ["select-pane", "-t", HUB_LEFT_PANE]);
}

/** Focus the right (harness) pane. */
export async function focusRight(): Promise<void> {
  await spawnTmux(HUB_SOCKET, ["select-pane", "-t", HUB_RIGHT_PANE]);
}

/**
 * Tear down the hub session, detaching the user. Inner-server sessions
 * (every harness/diff/shell session, plus `wt-hub-home`) are untouched
 * — only the outer chrome goes away.
 */
export async function killHub(): Promise<void> {
  await spawnTmux(HUB_SOCKET, ["kill-session", "-t", HUB_SESSION]);
}
