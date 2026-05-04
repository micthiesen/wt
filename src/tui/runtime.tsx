import { createCliRenderer } from "@opentui/core";
import { createRoot } from "@opentui/react";
import { QueryClientProvider } from "@tanstack/react-query";

import { actionRegistry } from "../core/actions.ts";
import { reapArchived } from "../core/archive.ts";
import { createLogger, flushLogger, setEventSink } from "../core/logger.ts";
import { reapOrphanedSessions } from "../core/tmux.ts";
import { listWorktrees } from "../core/worktree.ts";
import { reapWtState } from "../core/wtstate.ts";
import { createWtQueryClient } from "../state/index.ts";

import { App, type TuiExit } from "./app.tsx";
import { events } from "./events.ts";
import { attachFetchLogs } from "./fetch-log.ts";

const startupLog = createLogger("[startup]");

/**
 * Drop state.json + archive.json entries whose slug no longer exists
 * in `git worktree list`. Destroys deliberately leave both files alone
 * (so the row stays visually archived through the destroy without
 * flicker — see `removeWorktree`); fresh-start on re-create is handled
 * by `createWorktree`. This sweep is for ghosts left behind by external
 * removes (`git worktree remove` from the shell, repo blown away) or by
 * destroys whose target slug never gets re-created. Errors are
 * swallowed; a stale entry is a worse outcome than blocking startup.
 */
async function reapStartup(): Promise<void> {
  try {
    const wts = await listWorktrees();
    const live = new Set(wts.map((w) => w.slug));
    reapWtState(live);
    reapArchived(live);
    // Kill any tmux sessions whose slug no longer exists. Covers the
    // case where a worktree was removed externally (or in a prior wt
    // run that crashed before the destroy hook fired).
    await reapOrphanedSessions(live);
  } catch (err) {
    startupLog.warn("reap failed", { err: err instanceof Error ? err.message : String(err) });
  }
}

export async function runTui(): Promise<TuiExit> {
  // Forward logger.event.* into the activity-pane store. CLI runs leave
  // this unset, so event-style log calls there go to the file only.
  setEventSink((e) => {
    events.append(e);
  });

  const wtClient = createWtQueryClient();
  const detachFetchLogs = attachFetchLogs(wtClient.client);
  // Wait briefly for the SQLite cache to hydrate so the first paint
  // shows stale data instead of empty. If hydration takes longer than
  // the budget we render anyway and it'll swap in when ready. Reap
  // racing concurrently — it doesn't gate the first paint, but
  // resolving it before the wtState query observer kicks in saves an
  // immediate refetch.
  await Promise.all([
    Promise.race([
      wtClient.restored,
      new Promise<void>((r) => setTimeout(r, 150)),
    ]),
    reapStartup(),
  ]);

  const renderer = await createCliRenderer({
    exitOnCtrlC: false,
    targetFps: 60,
  });
  const root = createRoot(renderer);
  return new Promise<TuiExit>((resolve) => {
    root.render(
      <QueryClientProvider client={wtClient.client}>
        <App onExit={resolve} />
      </QueryClientProvider>,
    );
  }).finally(async () => {
    // Tear down listeners, timers, and the SQLite handle so the
    // process can exit cleanly. Each step may throw if an earlier
    // one already disposed state — we swallow so all three run.
    try {
      root.unmount();
    } catch (err) {
      void err;
    }
    try {
      renderer.destroy();
    } catch (err) {
      void err;
    }
    detachFetchLogs();
    setEventSink(null);
    wtClient.shutdown();
    // SIGTERM in-flight `claude -p` actions and await their drains
    // so we don't strand subprocesses (or truncate their log files)
    // when main.ts hits process.exit.
    await actionRegistry.shutdown();
    // Drain queued log writes before main.ts hits process.exit.
    await flushLogger();
  });
}
