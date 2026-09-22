import { createCliRenderer } from "@opentui/core";
import { createRoot } from "@opentui/react";
import { QueryClientProvider } from "@tanstack/react-query";
import { Cause, Data, Deferred, Effect, Fiber } from "effect";
import type * as EffectScope from "effect/Scope";

import { actionRegistry } from "../core/actions.ts";
import { causeMessage } from "../core/errors.ts";
import { reapArchived } from "../core/archive.ts";
import { recordWorktreeEdit } from "../core/automations.ts";
import { watchRegistry } from "../core/harness/claude/registry.ts";
import { config } from "../core/config.ts";
import { disposeDiffPool } from "../core/diff/pool.ts";
import { lockStatus } from "../core/locks.ts";
import { watchGithubEvents } from "../core/events/store.ts";
import { closeOpencodeDb, getHarness, HARNESSES } from "../core/harness/index.ts";
import { startCodexEventPolling } from "../core/harness/codex/events.ts";
import { disposeCodexDiscoveryWorker } from "../core/harness/codex/discovery.ts";
import { harnessTailRegistry } from "../core/harness/tail.ts";
import { startOpencodeEventPolling } from "../core/harness/opencode/events.ts";
import { createLogger, flushLogger, setEventSink } from "../core/logger.ts";
import { ensureManagerClaudeName, MANAGER_SLUG } from "../core/manager.ts";
import { killHarnessSession, listAllSessionsRaw } from "../core/tmux.ts";
import { reapDevServerFiles } from "../core/dev-server.ts";
import { reapDestroyLogs } from "../core/logs.ts";
import {
  sessionTailRegistry,
  setSessionSlugChangeSink,
  setSessionTriggerSink,
} from "../core/harness/claude/tail.ts";
import {
  RiftRebaseWatchSet,
  WorktreeWatchSet,
  watchLockDir,
  watchRebaseState,
  watchRefs,
  watchWorktreeRoot,
  watchWorktreesAdmin,
  watchWtStateFiles,
} from "../core/repo-watch.ts";
import { isRiftWorktree } from "../core/backend.ts";
import { attachInputLatencyProbe, startLoopLagProbe } from "../core/perf.ts";
import { reapShellLogs, shellTailRegistry } from "../core/shell-tail.ts";
import { reapOrphanedSessions } from "../core/tmux.ts";
import { listWorktrees } from "../core/worktree.ts";
import { readWtState, reapWtState } from "../core/wtstate.ts";
import { createWtQueryClient } from "../state/index.ts";
import { qk } from "../state/keys.ts";
import {
  fetchOriginNow,
  fetchOriginQuery,
  type TmuxSessionsData,
} from "../state/queries.ts";
import type { Worktree } from "../core/types.ts";
import type { QueryClient } from "@tanstack/react-query";

import { App, type TuiExit } from "./app.tsx";
import { TuiErrorBoundary } from "./error-boundary.tsx";
import { installProcessErrorCapture } from "./error-store.ts";
import { backfillActivityLog } from "./activity-backfill.ts";
import { events } from "./activity-log.ts";
import { closeAutoMergeRetries } from "./flows/auto-merge-retry.ts";
import { attachFetchLogs } from "./fetch-log.ts";
import { SESSION_SLOTS, SLOT_SLUGS } from "./sessions/slots.ts";
import { attachLoggerToasts } from "./toast.ts";
import { syncTerminalPalette } from "./terminal-palette.ts";

const startupLog = createLogger("[startup]");

const INVALIDATION_FLUSH_MS = 50;

class TuiRendererError extends Data.TaggedError("TuiRendererError")<{
  readonly cause: unknown;
  readonly message: string;
}> {}

class RuntimeCleanupError extends Data.TaggedError("RuntimeCleanupError")<{
  readonly kind: "resource" | "finalizer";
  readonly cause: unknown;
}> {}

/**
 * Query namespaces whose answers depend on refs in the main clone. Keep this
 * beside the refs watcher so a newly-added ref-backed source cannot silently
 * miss the push invalidation that makes branch.advanced observable.
 */
export function invalidateRefQueries(
  invalidate: (key: readonly unknown[]) => void,
): void {
  for (const key of [
    ["github"] as const,
    qk.reviewRequests(),
    ["wt"] as const,
    qk.wtState(),
    ["watchedBranchTips"] as const,
  ]) {
    invalidate(key);
  }
}

/**
 * Attach a resource to the current Effect scope. Cleanup failures are defects
 * of the cleanup itself, not a reason to abandon the rest of the scope: every
 * independently registered finalizer must still get its turn during shutdown.
 */
export function acquireRuntimeResource<A, E, R>(
  acquire: Effect.Effect<A, E, R>,
  release: (resource: A) => void | Promise<void>,
): Effect.Effect<A, E, R | EffectScope.Scope> {
  return Effect.acquireRelease(acquire, (resource) =>
    Effect.tryPromise({
      try: async () => {
        await release(resource);
      },
      catch: (cause) => new RuntimeCleanupError({ kind: "resource", cause }),
    }).pipe(
      Effect.catch((cause) =>
        Effect.sync(() => {
          startupLog.warn("runtime resource cleanup failed", {
            err:
              cause.cause instanceof Error
                ? cause.cause.message
                : String(cause.cause),
          });
        }),
      ),
    ),
  );
}

function acquireSyncResource<A>(
  acquire: () => A,
  release: (resource: A) => void | Promise<void>,
): Effect.Effect<A, never, EffectScope.Scope> {
  return acquireRuntimeResource(Effect.sync(acquire), release);
}

function addRuntimeFinalizer(
  release: () => void | Promise<void>,
): Effect.Effect<void, never, EffectScope.Scope> {
  return Effect.addFinalizer(() =>
    Effect.tryPromise({
      try: async () => {
        await release();
      },
      catch: (cause) => new RuntimeCleanupError({ kind: "finalizer", cause }),
    }).pipe(
      Effect.catch((cause) =>
        Effect.sync(() => {
          startupLog.warn("runtime finalizer failed", {
            err:
              cause.cause instanceof Error
                ? cause.cause.message
                : String(cause.cause),
          });
        }),
      ),
    ),
  );
}

type InvalidationJob =
  | { kind: "key"; key: readonly unknown[] }
  | { kind: "claudeHarnessSessions" }
  | { kind: "fetchOrigin"; force: boolean };

class InvalidationError extends Data.TaggedError("InvalidationError")<{
  readonly cause: unknown;
}> {}

const forkBestEffort = (run: () => Promise<unknown>): void => {
  Effect.runFork(
    Effect.tryPromise({
      try: run,
      catch: (cause) => new InvalidationError({ cause }),
    }).pipe(
      Effect.catch((error) =>
        Effect.sync(() => {
          startupLog.warn("invalidation failed", { err: causeMessage(error.cause) });
        }),
      ),
    ),
  );
};

class InvalidationScheduler {
  private readonly jobs = new Map<string, InvalidationJob>();
  private timer: Fiber.Fiber<void, never> | null = null;
  private disposed = false;

  constructor(private readonly client: QueryClient) {}

  key(key: readonly unknown[]): void {
    this.enqueue({ kind: "key", key }, `key:${JSON.stringify(key)}`);
  }

  claudeHarnessSessions(): void {
    this.enqueue({ kind: "claudeHarnessSessions" }, "claudeHarnessSessions");
  }

  fetchOrigin(opts: { force?: boolean } = {}): void {
    this.enqueue(
      { kind: "fetchOrigin", force: opts.force ?? false },
      "fetchOrigin",
    );
  }

  dispose(): void {
    this.disposed = true;
    if (this.timer !== null) Effect.runFork(Fiber.interrupt(this.timer));
    this.timer = null;
    this.jobs.clear();
  }

  private enqueue(job: InvalidationJob, id: string): void {
    if (this.disposed) return;
    this.jobs.set(id, job);
    if (this.timer !== null) return;
    let timer: Fiber.Fiber<void, never>;
    timer = Effect.runFork(
      Effect.sleep(`${INVALIDATION_FLUSH_MS} millis`).pipe(
        Effect.andThen(
          Effect.sync(() => {
            if (this.timer === timer) this.timer = null;
            this.flush();
          }),
        ),
        Effect.ensuring(
          Effect.sync(() => {
            if (this.timer === timer) this.timer = null;
          }),
        ),
      ),
    );
    this.timer = timer;
  }

  private flush(): void {
    if (this.disposed) return;
    const jobs = [...this.jobs.values()];
    this.jobs.clear();
    for (const job of jobs) {
      if (job.kind === "key") {
        forkBestEffort(() =>
          this.client.invalidateQueries({ queryKey: job.key }),
        );
      } else if (job.kind === "claudeHarnessSessions") {
        forkBestEffort(() =>
          this.client.invalidateQueries({
            predicate: (q) =>
              q.queryKey[0] === "harnessSessions" && q.queryKey[1] === "claude",
          }),
        );
      } else if (job.force) {
        forkBestEffort(fetchOriginNow);
      } else {
        forkBestEffort(() => this.client.fetchQuery(fetchOriginQuery()));
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
const reapStartupEffect: Effect.Effect<void, never> = Effect.gen(function* () {
    const wts = yield* listWorktrees();
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
    reapDevServerFiles(live);
    // Kill any tmux sessions whose slug no longer exists. Covers the
    // case where a worktree was removed externally (or in a prior wt
    // run that crashed before the destroy hook fired). Session slots
    // (the `.` and `,` bindings) own slugs outside the worktree
    // namespace — whitelist them here so the reaper doesn't kill them.
    const protectedSlugs = new Set(live);
    for (const slug of SLOT_SLUGS) protectedSlugs.add(slug);
    for (const slug of actionRegistry.persistedRemoteActionKeys()) {
      protectedSlugs.add(slug);
    }
    yield* reapOrphanedSessions(protectedSlugs);
    // One-time migration: the manager now lives as a NAMED claude
    // session (`manager~manager`) with its own conversation UUID —
    // the old bare `manager` primary shared main's conversation (same
    // cwd → same cwd-keyed UUID). A leftover primary-form session
    // would linger protected forever and confuse `m` (which no longer
    // attaches it), so kill it here. New code never creates it.
    ensureManagerClaudeName();
    const tmuxLive = yield* listAllSessionsRaw();
    if (tmuxLive.has(MANAGER_SLUG)) {
      startupLog.warn(
        "killing legacy primary-form manager session (shared main's conversation)",
      );
      yield* killHarnessSession(MANAGER_SLUG, "claude", null);
    }
    // Drop terminal action run dirs whose slug is gone OR that fall
    // beyond the rehydration window. Ordered before `boot` so the
    // boot scan only sees dirs we'll actually keep — saves a meta-
    // read per stale dir.
    actionRegistry.reapDirs(live);
    // Rehydrate action runs from disk + tmux. Picks up any action
    // session that was running when the previous wt exited (or
    // crashed) and re-attaches a live tail; finalizes runs whose
    // wrapper exited while wt was down.
    yield* actionRegistry.boot(live);
}).pipe(
  // Reaping is maintenance and must not prevent first paint. Keep the
  // failure policy from the legacy helper, but represent every async leg as
  // a typed Effect operation before applying that policy once at the edge.
  Effect.catchCause((cause) => Effect.sync(() => {
    startupLog.warn("reap failed", {
      err: Cause.pretty(cause),
    });
  })),
);

export const runTui = Effect.gen(function* () {
  // These process-wide registries may acquire handles lazily while the UI is
  // alive. Register their shutdown before startup begins so a later startup
  // failure still drains anything acquired in the meantime.
  yield* Effect.addFinalizer(() => flushLogger);
  yield* addRuntimeFinalizer(() => {
    sessionTailRegistry.stopAll();
    shellTailRegistry.stopAll();
    harnessTailRegistry.stopAll();
  });
  yield* Effect.addFinalizer(() => actionRegistry.shutdown());
  yield* Effect.addFinalizer(() => closeAutoMergeRetries);
  yield* addRuntimeFinalizer(closeOpencodeDb);
  yield* addRuntimeFinalizer(disposeCodexDiscoveryWorker);
  yield* Effect.addFinalizer(() => disposeDiffPool());

  // Gate the pane-feed store's debounce fiber to this render's
  // lifetime — same attach/detach discipline as `attachLoggerToasts`
  // below, so its 16ms coalescing fiber is interrupted at shutdown
  // instead of surviving a torn-down render tree.
  yield* acquireSyncResource(() => events.attach(), () => events.detach());
  // Restore the pane feeds from the daily logs FIRST (a restart must
  // not wipe the attention trail), then forward live logger.event.*
  // into the store. CLI runs leave the sink unset, so event-style log
  // calls there go to the file only.
  backfillActivityLog();
  // Seed the attention "seen" watermark alongside the backfill: the
  // events come back from the daily logs, the watermark from wtstate,
  // so already-handled lines re-appear dim instead of bright.
  events.markSeen(readWtState().attentionSeenTs);
  yield* acquireSyncResource(
    () => {
      setEventSink((e) => {
        events.append(e);
      });
    },
    () => setEventSink(null),
  );
  // Logger emits carrying `{ toast: true }` also flash in the footer.
  yield* acquireSyncResource(attachLoggerToasts, (detach) => detach());

  const wtClient = yield* acquireSyncResource(createWtQueryClient, (client) =>
    client.shutdown(),
  );
  const invalidations = yield* acquireSyncResource(
    () => new InvalidationScheduler(wtClient.client),
    (scheduler) => scheduler.dispose(),
  );
  yield* acquireSyncResource(
    () => attachFetchLogs(wtClient.client),
    (detach) => detach(),
  );
  // fs-watch the claude session registry so the per-session "busy /
  // idle" indicator in the claude row flips the instant claude rewrites
  // its state file, without waiting for the 5s polling backstop on
  // `claudeRegistryQuery`. Cheap: a single FSEvents subscription on the
  // top-level dir, no recursion. `.catch(noop)` swallows rejections
  // from invalidations that race a torn-down client during shutdown.
  yield* acquireSyncResource(
    () =>
      watchRegistry(() => {
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
        // A registry rewrite IS claude activity (turn start/end) — exactly
        // when Anthropic API utilization changes. `claudeUsage` was
        // otherwise poll-only; this rides the same fs.watch as everything
        // else in this callback instead of waiting out its 60s interval.
        invalidations.key(qk.claudeUsage());
      }),
    (stop) => stop(),
  );
  // Local git activity → query invalidations. Coarse refs watcher fires
  // on commits, fetches, pushes, branch creates/deletes (anything that
  // touches `<main>/.git/refs/`). Per-worktree dir watchers fire on
  // working-tree edits and flip the dirty badge without waiting for
  // staleTime. Active observers refetch; cold queries stay cold.
  yield* acquireSyncResource(
    () =>
      watchRefs(config.paths.mainClone, () => {
        invalidateRefQueries((key) => invalidations.key(key));
      }),
    (stop) => stop(),
  );
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
  if (config.github.events) {
    yield* acquireSyncResource(
      () =>
        watchGithubEvents(() => {
          invalidations.key(["github"]);
          invalidations.key(qk.reviewRequests());
          invalidations.fetchOrigin({ force: true });
        }),
      (stop) => stop(),
    );
  }
  // Worktree membership changes (`git worktree add/remove` from any
  // process — `wt new` in a shell, `/split` in a Claude session, the
  // detached destroy finishing) → refresh the worktree list. The refs
  // watcher can't see these: worktree admin lives under `.git/worktrees/`,
  // not `refs/`.
  yield* acquireSyncResource(
    () =>
      watchWorktreesAdmin(config.paths.mainClone, () => {
        invalidations.key(qk.worktrees());
      }),
    (stop) => stop(),
  );
  // Rift checkouts are independent clones that never touch
  // `.git/worktrees/`, so the admin watcher above can't see them. Watch
  // the worktree root itself for create/remove (harmlessly redundant for
  // git worktrees, which also land as subdirs there).
  yield* acquireSyncResource(
    () =>
      watchWorktreeRoot(config.paths.worktreeRoot, () => {
        invalidations.key(qk.worktrees());
      }),
    (stop) => stop(),
  );
  // Rebase state appearing/disappearing in a worktree (`rebase-merge/`
  // under its `.git/worktrees/<slug>/` admin dir) → refresh that slug's
  // conflict probe so the mid-rebase glyph flips on the event. This is
  // the only push signal for a rebase started OUTSIDE the engine (a
  // `/restack` conflict resolution, a hand rebase): refs don't move
  // until it finishes, and the engine's own runs are covered by the
  // lock watcher instead. Covers LINKED worktrees only.
  yield* acquireSyncResource(
    () =>
      watchRebaseState(config.paths.mainClone, (slug) => {
        invalidations.key(qk.wt(slug).conflictAny());
      }),
    (stop) => stop(),
  );
  // A rift checkout is an independent clone, so its rebase control dir is
  // `<slice>/.git/rebase-*` — invisible to the main-clone watcher above.
  // One watcher per rift slice on its own `.git`, reconciled below against
  // the rift subset of the worktree list, so the mid-rebase glyph flips on
  // a hand / `/restack` rebase there too.
  const riftRebaseWatchSet = yield* acquireSyncResource(
    () =>
      new RiftRebaseWatchSet((slug) => {
        invalidations.key(qk.wt(slug).conflictAny());
      }),
    (watchSet) => watchSet.dispose(),
  );
  // Cross-process state.json / archive.json writes (CLI stack ops, `wt
  // base set`, another wt instance) → refresh the matching query so
  // sections, fork-base records, and the archived set track external
  // mutations live.
  yield* acquireSyncResource(
    () =>
      watchWtStateFiles((file) => {
        const key = file === "state" ? qk.wtState() : qk.archive();
        invalidations.key(key);
      }),
    (stop) => stop(),
  );
  // Per-slug lock churn → refresh that slug's lock query. Acquire /
  // phase writes / release all land here, so the busy state is push-
  // based in both directions: a create's "pnpm install" phase appears
  // and clears the moment it happens (any process), instead of waiting
  // on the lock query's while-held poll — which never arms at all when
  // the lock appears after the query last fetched null. The release
  // side then chains through `useLockReleasedInvalidator`, which
  // refreshes the released slug's field queries.
  yield* acquireSyncResource(
    () =>
      watchLockDir(config.paths.lockDir, (slug) => {
        if (slug === "*") {
          // Event without a filename — can't target one slug; refresh the
          // whole per-worktree namespace rather than risk a stuck "busy". A
          // create/destroy may also have completed, so refresh the list too.
          invalidations.key(["wt"]);
          invalidations.key(qk.worktrees());
          return;
        }
        invalidations.key(qk.wt(slug).lock());
        // A lock that's now GONE means a create or destroy just finished, so
        // worktree membership may have changed — refresh the list. This is the
        // completion signal the fs-dir watchers can't give for a rift create:
        // its `.rift` marker (what makes the row discoverable) is written INSIDE
        // the new dir, after the worktree-root watcher already fired on the bare
        // dir appearing — so without this a CLI `wt new` row would only surface
        // on the next interval. Gated on release (lock gone) so mid-op phase
        // writes during a long restack don't churn the list.
        if (!lockStatus(slug)) invalidations.key(qk.worktrees());
      }),
    (stop) => stop(),
  );
  const worktreeWatchSet = yield* acquireSyncResource(
    () =>
      new WorktreeWatchSet((slug, area) => {
        // `.sst/` writes flip the deploy badge (deploys + removes always
        // write there); everything else is a working-tree edit → dirty.
        const key = area === "sst" ? qk.wt(slug).deploy() : qk.wt(slug).dirty();
        invalidations.key(key);
        // Feed the automations engine's settle window: any observed write
        // (tree edit or deploy churn) counts as "someone is working here".
        recordWorktreeEdit(slug);
      }),
    (watchSet) => watchSet.dispose(),
  );
  // Reconcile the per-worktree watcher set against the worktrees query.
  // Skip `isMain` — the main clone's tree is heavy (node_modules) and
  // the user works in worktrees, not trunk. Subscribe first so we never
  // miss a `set` event, then reconcile against the current cache for
  // the boot case where the persister already restored data.
  const reconcileWatchers = (wts: readonly Worktree[] | undefined): void => {
    if (!wts) return;
    const targets = wts
      .filter((w) => !w.isMain && w.path)
      .map((w) => ({ slug: w.slug, path: w.path }));
    worktreeWatchSet.reconcile(targets);
    // Only rift slices carry their own `.git` rebase state; a `.rift`
    // marker probe keeps the watcher set to the clones that need it.
    riftRebaseWatchSet.reconcile(targets.filter((t) => isRiftWorktree(t.path)));
  };
  yield* acquireSyncResource(
    () =>
      wtClient.client.getQueryCache().subscribe((event) => {
        if (event.type !== "updated") return;
        if (event.query.queryKey[0] !== "worktrees") return;
        reconcileWatchers(event.query.state.data as Worktree[] | undefined);
      }),
    (unsubscribe) => unsubscribe(),
  );
  reconcileWatchers(wtClient.client.getQueryData<Worktree[]>(qk.worktrees()));
  // Wait briefly for the SQLite cache to hydrate so the first paint
  // shows stale data instead of empty. If hydration takes longer than
  // the budget we render anyway and it'll swap in when ready. Reap
  // racing concurrently — it doesn't gate the first paint, but
  // resolving it before the wtState query observer kicks in saves an
  // immediate refetch.
  yield* Effect.all(
    [
      Effect.race(
        Effect.promise(() => wtClient.restored),
        Effect.sleep("150 millis"),
      ),
      reapStartupEffect,
    ],
    { concurrency: "unbounded" },
  );

  // Start Codex activity-event polling. Same pattern as opencode: the
  // getter reads from the query cache imperatively (no React) and is
  // safe to call from the interval callback outside the render tree.
  // Rollout changes invalidate both usage and the affected session states.
  // Include special slots explicitly: they are live tmux slugs but are not
  // members of the worktree inventory.
  yield* acquireSyncResource(
    () =>
      startCodexEventPolling(
        () => {
          const worktrees =
            wtClient.client.getQueryData<Worktree[]>(qk.worktrees()) ?? [];
          const tmux = wtClient.client.getQueryData<TmuxSessionsData>(
            qk.tmuxSessions(),
          );
          const liveCodex = new Set(tmux?.slugsByHarness.codex ?? []);
          const paths = new Map(
            SESSION_SLOTS.map((slot) => [slot.slug, slot.path]),
          );
          for (const wt of worktrees) paths.set(wt.slug, wt.path);
          return [...liveCodex]
            .flatMap((slug) => {
              const wtPath = paths.get(slug);
              const tmuxName = getHarness("codex").tmuxSessionName(slug, null);
              return wtPath ? [{
                slug,
                wtPath,
                sessionId: tmux?.harnessSessionIds[tmuxName] ?? null,
              }] : [];
            });
        },
        (slugs) => {
          invalidations.key(qk.codexUsage());
          for (const slug of slugs) {
            invalidations.key(qk.harnessSessions("codex", slug));
          }
        },
      ),
    (stop) => stop(),
  );

  // Start OpenCode activity-event polling. The getter reads from the
  // query cache imperatively (no React) so it's safe to call from the
  // interval callback outside the render tree. `onActivity` invalidates
  // `opencodeCost` for the same reason as codex above.
  if (!config.harness.hidden.has("opencode")) {
    yield* acquireSyncResource(
      () =>
        startOpencodeEventPolling(
          () => {
            const worktrees =
              wtClient.client.getQueryData<Worktree[]>(qk.worktrees()) ?? [];
            const tmux = wtClient.client.getQueryData<TmuxSessionsData>(
              qk.tmuxSessions(),
            );
            // Only scan slugs that have a live opencode tmux session.
            const liveOpencode = new Set(tmux?.slugsByHarness.opencode ?? []);
            return worktrees
              .filter((wt) => liveOpencode.has(wt.slug))
              .map((wt) => ({ slug: wt.slug, wtPath: wt.path }));
          },
          () => invalidations.key(qk.opencodeCost()),
        ),
      (stop) => stop(),
    );
  }

  // Wire the session tail's refresh triggers to the query cache. The
  // tailer is already reading every live Claude jsonl for the activity
  // pane; when it spots a `gh pr create` / `git push` &c it reports a
  // refresh target here and we invalidate the matching query right
  // away instead of waiting out its slow staleTime. `.catch` swallows
  // the race against a torn-down client during shutdown.
  yield* acquireSyncResource(
    () =>
      setSessionTriggerSink((target) => {
        if (target === "github") {
          invalidations.key(["github"]);
          invalidations.key(qk.reviewRequests());
        }
      }),
    () => setSessionTriggerSink(null),
  );
  // The session tail already watches every live claude jsonl for the
  // activity pane; this sink piggybacks on it to invalidate just the
  // affected slug's claude query so the row's last-activity age + queue
  // count snap on turn end instead of waiting out the 5s poll. Scoped
  // tightly — only `qk.wt(slug).claude()`, nothing global.
  yield* acquireSyncResource(
    () =>
      setSessionSlugChangeSink((slug) => {
        invalidations.key(qk.wt(slug).claude());
      }),
    () => setSessionSlugChangeSink(null),
  );

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
  yield* Effect.forkScoped(
    Effect.promise(() => wtClient.restored).pipe(
      Effect.andThen(
        Effect.sync(() => {
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
        }),
      ),
    ),
  );

  // Periodic `git fetch origin` backstop so origin-relative state
  // (behind counts, merged/gone badges) tracks the remote without a
  // manual `r`. Complements the webhook-marker fetch above: the marker
  // covers PR activity on the user's own branches, this interval covers
  // everything else (teammates pushing to main, repos without the
  // events daemon). The fetch itself is silent when nothing changed;
  // when refs DO move, the refs watcher fans out the invalidations —
  // no extra plumbing here. A single failure (offline, transient
  // network) is routine and only logged at warn; the next tick retries.
  // A RUN of failures is a different fact — sustained offline, a broken
  // remote — worth interrupting a scan for, so it escalates to
  // `log.attention` once it's no longer a blip.
  const FETCH_ORIGIN_INTERVAL_MS = 3 * 60 * 1000;
  const FETCH_ORIGIN_ATTENTION_THRESHOLD = 3;

  // Opt-in (`WT_PERF=1`) probe that logs whenever the single JS thread
  // is blocked long enough to drop a frame / stall a keypress. Used to
  // confirm the diff-pool offload actually unblocked the render thread.
  // The probe needs the renderer to attach, but renderer.destroy() must run
  // first so its final painted-frame callback cannot race a detached probe.
  // Register the later-acquired probe's slot before the renderer resource to
  // retain that teardown order while still covering renderer startup failure.
  let detachInputLatency: (() => void) | null = null;
  yield* addRuntimeFinalizer(() => detachInputLatency?.());

  yield* acquireSyncResource(startLoopLagProbe, (stop) => stop());
  let fetchOriginFailures = 0;
  yield* Effect.forkScoped(
    Effect.forever(
      Effect.sleep(`${FETCH_ORIGIN_INTERVAL_MS} millis`).pipe(
        Effect.andThen(
          Effect.tryPromise({
            try: fetchOriginNow,
            catch: (cause) => new InvalidationError({ cause }),
          }),
        ),
        Effect.tap(() => Effect.sync(() => { fetchOriginFailures = 0; })),
        Effect.catch((error) =>
          Effect.sync(() => {
            fetchOriginFailures++;
            const msg = causeMessage(error.cause);
            // Once, when the run stops being a blip; the counter resets on
            // the next success, so a second outage escalates again.
            if (fetchOriginFailures === FETCH_ORIGIN_ATTENTION_THRESHOLD) {
              startupLog.attention.warn(
                `periodic origin fetch has failed ${fetchOriginFailures} times in a row: ${msg}`,
              );
            } else {
              startupLog.warn("periodic origin fetch failed", { err: msg });
            }
          }),
        ),
      ),
    ),
  );

  // From the moment the renderer owns the terminal, Bun's default
  // uncaughtException/unhandledRejection reporters would print raw
  // stack traces over the panes. Install the capture (ring + file log +
  // error overlay; keep-alive semantics documented in error-store.ts)
  // for exactly the renderer's lifetime — detached by the scope after
  // `renderer.destroy()`, so late-teardown errors fall through to plain
  // stderr and main.ts's catch (the crash-rollback path) as before.
  yield* acquireSyncResource(installProcessErrorCapture, (detach) => detach());
  const renderer = yield* acquireRuntimeResource(
    Effect.tryPromise({
      try: () =>
        createCliRenderer({
          exitOnCtrlC: false,
          // No targetFps override: it only applies in the renderer's "live"
          // (continuous) mode, which wt never enters now that no Timeline /
          // requestAnimationFrame users exist (see spinner.tsx). If some
          // future dependency re-arms live mode, the default 30fps halves
          // the damage vs the 60 this used to pass. On-demand keypress
          // frames are throttled by maxFps (60), not this.
          // OpenTUI installs its own uncaughtException/unhandledRejection
          // hook that console.errors the stack and (by default) pops its
          // debug console overlay over the panes — the error overlay above
          // owns that surface now, so keep OpenTUI's from fighting it.
          openConsoleOnError: false,
          // wt owns its keyboard entirely (`useKeyboard` → the dispatch
          // chain in tui/keyboard/) and has no focusable widgets — every
          // text input is drawn and keyed by hand. OpenTUI's autoFocus,
          // left on, focuses the first focusable ANCESTOR of whatever gets
          // left-clicked, which is always a scrollbox, and a focused
          // scrollbox installs a GLOBAL keypress handler that scrolls 1/5
          // of a viewport on j/k/h/l/arrows/PgUp/PgDn — modifiers ignored,
          // so Ctrl+J/K hit it too. One stray click (focusing the terminal
          // window is enough) and from then on every j moves the cursor
          // AND jerks some pane four rows, often a pane the key has
          // nothing to do with. That's the "it gets weird after a while"
          // bug: the trigger is a mouse click long since forgotten.
          autoFocus: false,
      }),
      catch: (cause) =>
        new TuiRendererError({
          cause,
          message: cause instanceof Error ? cause.message : String(cause),
        }),
    }),
    (activeRenderer) => activeRenderer.destroy(),
  );
  yield* syncTerminalPalette(renderer);
  // Frame-side half of the WT_PERF input-latency probe (the keypress
  // side is `markKeypress()` in App's keyboard dispatch). No-op pair
  // when the env var is unset.
  detachInputLatency = attachInputLatencyProbe(renderer);
  const root = yield* acquireSyncResource(
    () => createRoot(renderer),
    (activeRoot) => activeRoot.unmount(),
  );
  // A dying terminal (tmux kill-session/-server, window close, SSH drop)
  // delivers SIGHUP — without an explicit handler this process SURVIVES
  // it (opentui's raw-mode stdin keeps the loop alive), reparents to
  // launchd, and keeps polling GitHub and writing duplicate attention
  // lines forever. One probe sweep left 33 of these burning ~20% CPU
  // each. Route the signal through the same resolve the quit key uses
  // so the full teardown below runs; the unref'd force-exit backstop
  // covers a teardown that wedges (which would re-create the leak this
  // handler exists to prevent).
  const exit = yield* Deferred.make<TuiExit>();
  const resolve = (value: TuiExit): void => {
    Deferred.doneUnsafe(exit, Effect.succeed(value));
  };
  yield* acquireSyncResource(
    () => {
      const onHangup = () => {
        // File-log the reason first: distinguishing a hangup exit from a
        // normal quit is exactly what post-mortems of the orphan leak
        // needed. The graceful path flushes this when the scope closes;
        // if only the force-exit lands there'll be no flush — acceptable,
        // the absence of a subsequent clean-shutdown line IS the signal.
        startupLog.warn("terminal hangup (SIGHUP/SIGTERM) — tearing down");
        const force = setTimeout(() => process.exit(129), 2500);
        force.unref();
        resolve({ kind: "quit" });
      };
      process.on("SIGHUP", onHangup);
      process.on("SIGTERM", onHangup);
      return onHangup;
    },
    (onHangup) => {
      process.off("SIGHUP", onHangup);
      process.off("SIGTERM", onHangup);
    },
  );
  yield* Effect.sync(() => {
    root.render(
      <QueryClientProvider client={wtClient.client}>
        {/* Render-error capture: a crash in the app tree lands in the
            same error ring (never stdout/stderr) and renders a minimal
            crash screen with retry/quit instead of unmounting to a
            garbled terminal. */}
        <TuiErrorBoundary onExit={resolve}>
          <App onExit={resolve} />
        </TuiErrorBoundary>
      </QueryClientProvider>,
    );
  });
  return yield* Deferred.await(exit);
}).pipe(Effect.scoped);
