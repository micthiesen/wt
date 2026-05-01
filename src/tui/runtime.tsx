import { createCliRenderer } from "@opentui/core";
import { createRoot } from "@opentui/react";
import { QueryClientProvider } from "@tanstack/react-query";

import { createLogger, flushLogger, setEventSink } from "../core/logger.ts";
import { listWorktrees } from "../core/worktree.ts";
import { reapWtState } from "../core/wtstate.ts";
import { createWtQueryClient } from "../state/index.ts";

import { App, type TuiExit } from "./app.tsx";
import { events } from "./events.ts";
import { attachFetchLogs } from "./fetch-log.ts";

const startupLog = createLogger("[startup]");

/**
 * Drop state.json entries whose slug no longer exists in `git worktree
 * list`. Catches the case where a worktree is removed outside `wt`
 * (manual `git worktree remove`, repo blown away, etc.) — `lifecycle`
 * already reaps on its own destroys, so this is the leak-fixer for
 * external operations. Errors are swallowed; a stale state entry is
 * a worse outcome than blocking startup.
 */
async function reapState(): Promise<void> {
  try {
    const wts = await listWorktrees();
    reapWtState(new Set(wts.map((w) => w.slug)));
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
    reapState(),
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
    // Drain queued log writes before main.ts hits process.exit.
    await flushLogger();
  });
}
