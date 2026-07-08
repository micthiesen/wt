import { createCliRenderer } from "@opentui/core";
import { createRoot } from "@opentui/react";
import { QueryClientProvider } from "@tanstack/react-query";

import { actionRegistry } from "../core/actions.ts";
import { reapArchived } from "../core/archive.ts";
import { watchRegistry } from "../core/claude-registry.ts";
import { config } from "../core/config.ts";
import { disposeDiffPool } from "../core/diff/pool.ts";
import { watchGithubEvents } from "../core/events/store.ts";
import { closeOpencodeDb, HARNESSES } from "../core/harness/index.ts";
import { startCodexEventPolling } from "../core/harness/codex-events.ts";
import { harnessTailRegistry } from "../core/harness/harness-tail.ts";
import { startOpencodeEventPolling } from "../core/harness/opencode-events.ts";
import { createLogger, flushLogger, setEventSink } from "../core/logger.ts";
import { reapDestroyLogs } from "../core/logs.ts";
import {
  sessionTailRegistry,
  setSessionSlugChangeSink,
  setSessionTriggerSink,
} from "../core/session-tail.ts";
import {
  WorktreeWatchSet,
  watchRefs,
  watchWorktreesAdmin,
  watchWtStateFiles,
} from "../core/repo-watch.ts";
import { startLoopLagProbe } from "../core/perf.ts";
import { reapShellLogs, shellTailRegistry } from "../core/shell-tail.ts";
import { reapOrphanedSessions } from "../core/tmux.ts";
import { listWorktrees } from "../core/worktree.ts";
import { reapWtState } from "../core/wtstate.ts";
import { createWtQueryClient } from "../state/index.ts";
import { qk } from "../state/keys.ts";
import { fetchOriginNow, fetchOriginQuery, type TmuxSessionsData } from "../state/queries.ts";
import type { Worktree } from "../core/types.ts";
import type { QueryClient } from "@tanstack/react-query";

import { App, type TuiExit } from "./app.tsx";
import { events } from "./events.ts";
import { attachFetchLogs } from "./fetch-log.ts";
import { SLOT_SLUGS } from "./session-slots.ts";

const startupLog = createLogger("[startup]");

const INVALIDATION_FLUSH_MS = 50;

type InvalidationJob =
  | { kind: "key"; key: readonly unknown[] }
  | { kind: "claudeHarnessSessions" }
  | { kind: "fetchOrigin"; force: boolean };

class InvalidationScheduler {
  private readonly jobs = new Map<string, InvalidationJob>();
  private timer: Timer | null = null;
  private disposed = false;

  constructor(private readonly client: QueryClient) {}

  key(key: readonly unknown[]): void {
    this.enqueue({ kind: "key", key }, `key:${JSON.stringify(key)}`);
  }

  claudeHarnessSessions(): void {
    this.enqueue({ kind: "claudeHarnessSessions" }, "claudeHarnessSessions");
  }

  fetchOrigin(opts: { force?: boolean } = {}): void {
    this.enqueue({ kind: "fetchOrigin", force: opts.force ?? false }, "fetchOrigin");
  }

  dispose(): void {
    this.disposed = true;
    if (this.timer !== null) clearTimeout(this.timer);
    this.timer = null;
    this.jobs.clear();
  }

  private enqueue(job: InvalidationJob, id: string): void {
    if (this.disposed) return;
    this.jobs.set(id, job);
    if (this.timer !== null) return;
    this.timer = setTimeout(() => this.flush(), INVALIDATION_FLUSH_MS);
  }

  private flush(): void {
    this.timer = null;
    if (this.disposed) return;
    const jobs = [...this.jobs.values()];
    this.jobs.clear();
    for (const job of jobs) {
      if (job.kind === "key") {
        this.client.invalidateQueries({ queryKey: job.key }).catch(() => {});
      } else if (job.kind === "claudeHarnessSessions") {
        this.client
          .invalidateQueries({
            predicate: (q) =>
              q.queryKey[0] === "harnessSessions" &&
              q.queryKey[1] === "claude",
          })
          .catch(() => {});
      } else if (job.force) {
        fetchOriginNow().catch(() => {});
      } else {
        this.client.fetchQuery(fetchOriginQuery()).catch(() => {});
      }
    }
  }
}

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
    const liveHarnessSlugs = new Set(live);
    for (const slug of SLOT_SLUGS) liveHarnessSlugs.add(slug);
    reapWtState(live);
    reapArchived(live);
    for (const harness of HARNESSES) harness.reapState(liveHarnessSlugs);
    // Drop pipe-pane shell logs and `<slug>-*.log` destroy logs whose
    // slug no longer exists — keeps `~/.cache/wt/` from accumulating
    // ghosts from worktrees long since destroyed. Live-slug logs are
    // always kept (a destroy in flight may still be writing).
    reapShellLogs(live);
    reapDestroyLogs(live);
    // Kill any tmux sessions whose slug no longer exists. Covers the
    // case where a worktree was removed externally (or in a prior wt
    // run that crashed before the destroy hook fired). Session slots
    // (the `.` and `,` bindings) own slugs outside the worktree
    // namespace — whitelist them here so the reaper doesn't kill them.
    const protectedSlugs = new Set(live);
    for (const slug of SLOT_SLUGS) protectedSlugs.add(slug);
    await reapOrphanedSessions(protectedSlugs);
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
  const invalidations = new InvalidationScheduler(wtClient.client);
  const detachFetchLogs = attachFetchLogs(wtClient.client);
  // fs-watch the claude session registry so the per-session "busy /
  // idle" indicator in the claude row flips the instant claude rewrites
  // its state file, without waiting for the 5s polling backstop on
  // `claudeRegistryQuery`. Cheap: a single FSEvents subscription on the
  // top-level dir, no recursion. `.catch(noop)` swallows rejections
  // from invalidations that race a torn-down client during shutdown.
  const stopRegistryWatch = watchRegistry(() => {
    invalidations.key(qk.claudeRegistry());
    // Claude's working/asking/waiting state is baked into the
    // `harnessSessions` discovery cache (it reads the registry inside
    // its queryFn), and that cache — not `claudeRegistry` — now drives
    // the list-pane glyph tint and the details AI row. The registry
    // write that just fired IS that state changing, so refresh the
    // claude discovery too; otherwise the tint would only update on
    // spawn/kill/manual-refresh. Scoped to claude + active observers
    // (the live-slug fan-out), so it stays cheap.
    invalidations.claudeHarnessSessions();
    // A registry write also means a claude process started or exited,
    // which is exactly when the tmux session set changes — refresh it
    // here so the session badges flip on the event instead of the
    // (now slower) polling backstop.
    invalidations.key(qk.tmuxSessions());
  });
  // Local git activity → query invalidations. Coarse refs watcher fires
  // on commits, fetches, pushes, branch creates/deletes (anything that
  // touches `<main>/.git/refs/`). Per-worktree dir watchers fire on
  // working-tree edits and flip the dirty badge without waiting for
  // staleTime. Active observers refetch; cold queries stay cold.
  const stopRefsWatch = watchRefs(config.paths.mainClone, () => {
    invalidations.key(["github"]);
    invalidations.key(qk.reviewRequests());
    invalidations.key(["wt"]);
    invalidations.key(qk.wtState());
  });
  // GitHub webhook deliveries → query invalidation. The `wt events` daemon
  // rewrites a marker file after each refetch; watching it is the push
  // counterpart to the refs watcher above, scoped to PR / check / merge-
  // queue state. Only armed when `[github.events]` is configured; otherwise
  // the github query stays on its poll cadence. `keepPreviousData` keeps the
  // pane painted across the refetch.
  //
  // Each delivery also forces an origin refresh: a PR merge or default-branch
  // push advances origin/main, and without a fetch the behind-counts and
  // merged/gone badges sit on stale local refs until a manual `r`. The
  // fetch's ref updates then flow back through the refs watcher above — one
  // push event drives the whole cascade.
  const stopGithubEventsWatch = config.github.events
    ? watchGithubEvents(() => {
        invalidations.key(["github"]);
        invalidations.key(qk.reviewRequests());
        invalidations.fetchOrigin({ force: true });
      })
    : null;
  // Worktree membership changes (`git worktree add/remove` from any
  // process — `wt new` in a shell, `/split` in a Claude session, the
  // detached destroy finishing) → refresh the worktree list. The refs
  // watcher can't see these: worktree admin lives under `.git/worktrees/`,
  // not `refs/`.
  const stopWorktreesAdminWatch = watchWorktreesAdmin(
    config.paths.mainClone,
    () => {
      invalidations.key(qk.worktrees());
    },
  );
  // Cross-process state.json / archive.json writes (CLI stack ops, `wt
  // base set`, another wt instance) → refresh the matching query so
  // sections, stack manifests, and the archived set track external
  // mutations live.
  const stopWtStateWatch = watchWtStateFiles((file) => {
    const key = file === "state" ? qk.wtState() : qk.archive();
    invalidations.key(key);
  });
  const worktreeWatchSet = new WorktreeWatchSet((slug, area) => {
    // `.sst/` writes flip the deploy badge (deploys + removes always
    // write there); everything else is a working-tree edit → dirty.
    const key =
      area === "sst" ? qk.wt(slug).deploy() : qk.wt(slug).dirty();
    invalidations.key(key);
  });
  // Reconcile the per-worktree watcher set against the worktrees query.
  // Skip `isMain` — the main clone's tree is heavy (node_modules) and
  // the user works in worktrees, not trunk. Subscribe first so we never
  // miss a `set` event, then reconcile against the current cache for
  // the boot case where the persister already restored data.
  const reconcileWatchers = (wts: readonly Worktree[] | undefined): void => {
    if (!wts) return;
    worktreeWatchSet.reconcile(
      wts
        .filter((w) => !w.isMain && w.path)
        .map((w) => ({ slug: w.slug, path: w.path })),
    );
  };
  const unsubWorktrees = wtClient.client.getQueryCache().subscribe((event) => {
    if (event.type !== "updated") return;
    if (event.query.queryKey[0] !== "worktrees") return;
    reconcileWatchers(event.query.state.data as Worktree[] | undefined);
  });
  reconcileWatchers(wtClient.client.getQueryData<Worktree[]>(qk.worktrees()));
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

  // Start Codex activity-event polling. Same pattern as opencode: the
  // getter reads from the query cache imperatively (no React) and is
  // safe to call from the interval callback outside the render tree.
  const stopCodexEvents = startCodexEventPolling(() => {
    const worktrees = wtClient.client.getQueryData<Worktree[]>(qk.worktrees()) ?? [];
    const tmux = wtClient.client.getQueryData<TmuxSessionsData>(qk.tmuxSessions());
    const liveCodex = new Set(tmux?.slugsByHarness.codex ?? []);
    return worktrees
      .filter((wt) => liveCodex.has(wt.slug))
      .map((wt) => ({ slug: wt.slug, wtPath: wt.path }));
  });

  // Start OpenCode activity-event polling. The getter reads from the
  // query cache imperatively (no React) so it's safe to call from the
  // interval callback outside the render tree.
  const stopOpencodeEvents = startOpencodeEventPolling(() => {
    const worktrees = wtClient.client.getQueryData<Worktree[]>(qk.worktrees()) ?? [];
    const tmux = wtClient.client.getQueryData<TmuxSessionsData>(qk.tmuxSessions());
    // Only scan slugs that have a live opencode tmux session.
    const liveOpecode = new Set(tmux?.slugsByHarness.opencode ?? []);
    return worktrees
      .filter((wt) => liveOpecode.has(wt.slug))
      .map((wt) => ({ slug: wt.slug, wtPath: wt.path }));
  });

  // Wire the session tail's refresh triggers to the query cache. The
  // tailer is already reading every live Claude jsonl for the activity
  // pane; when it spots a `gh pr create` / `git push` &c it reports a
  // refresh target here and we invalidate the matching query right
  // away instead of waiting out its slow staleTime. `.catch` swallows
  // the race against a torn-down client during shutdown.
  setSessionTriggerSink((target) => {
    if (target === "github") {
      invalidations.key(["github"]);
      invalidations.key(qk.reviewRequests());
    }
  });
  // The session tail already watches every live claude jsonl for the
  // activity pane; this sink piggybacks on it to invalidate just the
  // affected slug's claude query so the row's last-activity age + queue
  // count snap on turn end instead of waiting out the 5s poll. Scoped
  // tightly — only `qk.wt(slug).claude()`, nothing global.
  setSessionSlugChangeSink((slug) => {
    invalidations.key(qk.wt(slug).claude());
  });

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

  // Prune superseded `["github", <branches>]` entries. The PR query is
  // keyed by the full sorted branch list, so every worktree-set change
  // strands the previous key. Stale entries are pure dead weight —
  // reads are exact-key only (`keepPreviousData` feeds placeholders
  // from the observer's own prior result, not the cache) — yet they
  // persist for 30 days, re-hydrate on every boot, and get walked by
  // every optimistic `["github"]` patch. Keep the newest entry (warm
  // data for first paint; if the branch set changed while wt was down
  // this converges on the next boot) plus anything actively observed.
  // After `restored` so the sweep sees the fully hydrated set.
  void wtClient.restored.then(() => {
    const githubEntries = wtClient.client
      .getQueryCache()
      .findAll({ queryKey: ["github"] })
      .sort((a, b) => b.state.dataUpdatedAt - a.state.dataUpdatedAt);
    const stale = githubEntries
      .slice(1)
      .filter((q) => q.getObserversCount() === 0);
    for (const query of stale) wtClient.evict(query.queryKey);
    if (stale.length > 0) {
      startupLog.debug("pruned superseded github cache entries", {
        pruned: stale.length,
        kept: githubEntries.length - stale.length,
      });
    }
  });

  // Periodic `git fetch origin` backstop so origin-relative state
  // (behind counts, merged/gone badges) tracks the remote without a
  // manual `r`. Complements the webhook-marker fetch above: the marker
  // covers PR activity on the user's own branches, this interval covers
  // everything else (teammates pushing to main, repos without the
  // events daemon). The fetch itself is silent when nothing changed;
  // when refs DO move, the refs watcher fans out the invalidations —
  // no extra plumbing here. Errors (offline, transient network) are
  // swallowed; the next tick retries.
  const FETCH_ORIGIN_INTERVAL_MS = 3 * 60 * 1000;
  const fetchOriginTimer = setInterval(() => {
    fetchOriginNow().catch(() => {});
  }, FETCH_ORIGIN_INTERVAL_MS);

  // Opt-in (`WT_PERF=1`) probe that logs whenever the single JS thread
  // is blocked long enough to drop a frame / stall a keypress. Used to
  // confirm the diff-pool offload actually unblocked the render thread.
  const stopLoopLagProbe = startLoopLagProbe();

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
    invalidations.dispose();
    clearInterval(fetchOriginTimer);
    stopLoopLagProbe();
    disposeDiffPool();
    stopRegistryWatch();
    stopRefsWatch();
    stopGithubEventsWatch?.();
    stopWorktreesAdminWatch();
    stopWtStateWatch();
    unsubWorktrees();
    worktreeWatchSet.dispose();
    stopCodexEvents();
    stopOpencodeEvents();
    setEventSink(null);
    // Must null the trigger sink BEFORE `wtClient.shutdown()`: a
    // debounce timer can still be pending here, and nulling the sink
    // first makes its late fire a no-op. Reordering these two lines
    // reintroduces a window where the timer invalidates queries on a
    // torn-down client (the `.catch(() => {})` on the sink only papers
    // over it).
    setSessionTriggerSink(null);
    setSessionSlugChangeSink(null);
    wtClient.shutdown();
    // Close the harness-owned read handles (opencode's read-only
    // SQLite handle is the only one today). No-op for harnesses
    // without persistent handles.
    try {
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
    harnessTailRegistry.stopAll();
    // Drain queued log writes before main.ts hits process.exit.
    await flushLogger();
  });
}
