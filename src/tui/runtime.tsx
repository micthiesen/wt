import { createCliRenderer } from "@opentui/core";
import { createRoot } from "@opentui/react";
import { QueryClientProvider } from "@tanstack/react-query";

import { actionRegistry } from "../core/actions.ts";
import { reapArchived } from "../core/archive.ts";
import { watchRegistry } from "../core/claude-registry.ts";
import { HARNESSES } from "../core/harness/index.ts";
import { createLogger, flushLogger, setEventSink } from "../core/logger.ts";
import { reapDestroyLogs } from "../core/logs.ts";
import { sessionTailRegistry } from "../core/session-tail.ts";
import { reapShellLogs, shellTailRegistry } from "../core/shell-tail.ts";
import { reapOrphanedSessions, WT_SOURCE_SLUG } from "../core/tmux.ts";
import { listWorktrees } from "../core/worktree.ts";
import { reapWtState } from "../core/wtstate.ts";
import { createWtQueryClient } from "../state/index.ts";
import { qk } from "../state/keys.ts";

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
    for (const harness of HARNESSES) harness.reapState(live);
    // Drop pipe-pane shell logs and `<slug>-*.log` destroy logs whose
    // slug no longer exists — keeps `~/.cache/wt/` from accumulating
    // ghosts from worktrees long since destroyed. Live-slug logs are
    // always kept (a destroy in flight may still be writing).
    reapShellLogs(live);
    reapDestroyLogs(live);
    // Kill any tmux sessions whose slug no longer exists. Covers the
    // case where a worktree was removed externally (or in a prior wt
    // run that crashed before the destroy hook fired). The wt-source
    // shell session (`.` binding) lives on a sentinel slug outside the
    // worktree namespace, so add it explicitly to keep the reaper from
    // killing it.
    const liveWithSource = new Set(live);
    liveWithSource.add(WT_SOURCE_SLUG);
    await reapOrphanedSessions(liveWithSource);
    // Drop terminal action run dirs whose slug is gone OR that fall
    // beyond the rehydration window. Ordered before `boot` so the
    // boot scan only sees dirs we'll actually keep — saves a meta-
    // read per stale dir.
    actionRegistry.reapDirs(live);
    // Rehydrate action runs from disk + tmux. Picks up any action
    // session that was running when the previous wt exited (or
    // crashed) and re-attaches a live tail; finalizes runs whose
    // wrapper exited while wt was down.
    await actionRegistry.boot(live);
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
  // fs-watch the claude session registry so the per-session "busy /
  // idle" indicator in the claude row flips the instant claude rewrites
  // its state file, without waiting for the 5s polling backstop on
  // `claudeRegistryQuery`. Cheap: a single FSEvents subscription on the
  // top-level dir, no recursion. `.catch(noop)` swallows rejections
  // from invalidations that race a torn-down client during shutdown.
  const stopRegistryWatch = watchRegistry(() => {
    wtClient.client
      .invalidateQueries({ queryKey: qk.claudeRegistry() })
      .catch(() => {});
  });
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

  // Evict orphaned cache entries whose key shape changed across a wt
  // upgrade. Without this, an entry persisted under an old key sits in
  // memory after `restoreQueries` pre-warms it, where prefix-matching
  // mutation filters can mistake it for a current entry and feed the
  // wrong data shape to a patch helper. Cheap to keep this list as a
  // small append-only ledger — the alternative is bumping CACHE_BUSTER
  // and nuking every persisted entry, AI summaries included.
  const ORPHANED_KEYS: ReadonlyArray<readonly unknown[]> = [
    // v0.x: `reviewRequests` was briefly keyed `["github", "reviewRequests"]`
    // before moving off the `["github"]` prefix to avoid the
    // `setQueriesData(filter, patch)` shape mismatch in `patchPullRequest`.
    ["github", "reviewRequests"],
  ];
  for (const key of ORPHANED_KEYS) wtClient.evict(key);

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
    stopRegistryWatch();
    setEventSink(null);
    wtClient.shutdown();
    // Close the harness-owned read handles (opencode's read-only
    // SQLite handle is the only one today). No-op for harnesses
    // without persistent handles.
    try {
      const { closeOpencodeDb } = await import("../core/harness/index.ts");
      closeOpencodeDb();
    } catch (err) {
      void err;
    }
    // Detach from in-flight actions: close tails + done watchers so
    // we don't dangle file handles, but leave the tmux-supervised
    // wrappers running. The next `wt` invocation rehydrates them via
    // `actionRegistry.boot` — that's the whole point of moving
    // actions onto tmux.
    await actionRegistry.shutdown();
    // Close all jsonl + pipe-pane watchers + drop tailer state.
    sessionTailRegistry.stopAll();
    shellTailRegistry.stopAll();
    // Drain queued log writes before main.ts hits process.exit.
    await flushLogger();
  });
}
