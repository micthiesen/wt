/**
 * One shared non-throwing tmux spawn helper for every hub module,
 * mirroring `core/tmux/process.ts`'s `runTmux` but parameterized on
 * socket (hub mode talks to both the outer `wt-hub` socket and the
 * inner `wt` socket) and surfacing stdout too (needed for
 * `list-panes`/`display-message` queries, not just fire-and-forget
 * commands).
 */
import { homedir } from "node:os";

import { createLogger } from "../logger.ts";
import { run } from "../proc.ts";

const log = createLogger("[hub]");

export type TmuxResult = { code: number; stdout: string; stderr: string };

/**
 * Run `tmux -L <socket> <args...>`, capturing output instead of
 * throwing. `cwd` is pinned to `homedir()` rather than whatever
 * `run()`'s default (`config.paths.mainClone`) would give — the first
 * client to touch a socket forks its tmux server and that server
 * inherits the client's cwd for its whole life (see `tmuxClientCwd` in
 * `core/tmux/attach.ts` for the full story on why a worktree-rooted cwd
 * is dangerous here; a home directory never gets deleted out from under
 * a running server). Non-empty stderr is logged at debug so genuine
 * tmux failures aren't silently dropped, without promoting routine
 * no-ops (e.g. `has-session` on a missing session) to warnings —
 * callers decide what a given failure means.
 */
export async function spawnTmux(
  socket: string,
  args: readonly string[],
): Promise<TmuxResult> {
  const r = await run(["tmux", "-L", socket, ...args], { cwd: homedir() });
  if (r.stderr.trim()) {
    log.debug("tmux stderr", { socket, args: args.join(" "), stderr: r.stderr.trim() });
  }
  return { code: r.exitCode, stdout: r.stdout, stderr: r.stderr };
}
