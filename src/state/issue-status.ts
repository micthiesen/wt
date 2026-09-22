import type { QueryClient } from "@tanstack/react-query";
import { Duration, Effect } from "effect";
import { actionRegistry, type ActionRun } from "../core/actions.ts";
import { config } from "../core/config.ts";
import type { IssueStatuses } from "../core/issue-status.ts";

export type ExpectedIssueStatus = { status: string; token: symbol };

/** A transient projection, never server data or durable state. Per-ID ownership
 * prevents one failing action from rolling back a different issue's update. */
export function createIssueStatusExpectations() {
  let snapshot: ReadonlyMap<string, ExpectedIssueStatus> = new Map();
  const listeners = new Set<() => void>();
  const emit = () => { for (const listener of listeners) listener(); };
  return {
    getSnapshot: () => snapshot,
    subscribe: (listener: () => void) => { listeners.add(listener); return () => { listeners.delete(listener); }; },
    begin(id: string, status: string) {
      const token = Symbol(id);
      snapshot = new Map(snapshot).set(id, { status, token });
      emit();
      return token;
    },
    clear(id: string, token: symbol) {
      if (snapshot.get(id)?.token !== token) return;
      const next = new Map(snapshot);
      next.delete(id);
      snapshot = next;
      emit();
    },
  };
}

type Expectations = ReturnType<typeof createIssueStatusExpectations>;
const clients = new WeakMap<QueryClient, Expectations>();
export function issueStatusExpectations(qc: QueryClient): Expectations {
  let store = clients.get(qc);
  if (!store) { store = createIssueStatusExpectations(); clients.set(qc, store); }
  return store;
}

type ActionRegistryReader = Pick<typeof actionRegistry, "getSnapshot" | "subscribe">;

/** Exact run identity, with an immediate check for a command that already ended. */
const awaitAction = Effect.fnUntraced(function* (run: ActionRun, registry: ActionRegistryReader) {
  let unsubscribe = () => {};
  yield* Effect.addFinalizer(() => Effect.sync(() => unsubscribe()));
  return yield* Effect.callback<ActionRun>((resume) => {
    const check = () => {
      const current = registry.getSnapshot().get(run.slug);
      if (!current || current.runDir !== run.runDir) {
        resume(Effect.succeed({ ...run, status: "killed" }));
      } else if (current.status !== "running") resume(Effect.succeed(current));
    };
    unsubscribe = registry.subscribe(check);
    check();
    return Effect.sync(unsubscribe);
  });
});

/** Keep the expected value until the tracked command actually succeeds AND a
 * live read catches up. Failed/cancelled actions expose unmodified server data.
 * The guard is bounded and the final refresh reconciles ambiguous failures. */
export const trackIssueStatusAction = Effect.fn("trackIssueStatusAction")(function* (
  qc: QueryClient,
  run: ActionRun,
  id: string,
  status: string,
  registry: ActionRegistryReader = actionRegistry,
  settleMs = 12_000,
) {
  const store = issueStatusExpectations(qc);
  const token = yield* Effect.sync(() => store.begin(id, status));
  yield* Effect.addFinalizer(() => Effect.sync(() => store.clear(id, token)));
  const result = yield* awaitAction(run, registry);
  if (result.status !== "succeeded") return;
  let unsubscribe = () => {};
  yield* Effect.addFinalizer(() => Effect.sync(() => unsubscribe()));
  const caughtUp = Effect.callback<void>((resume) => {
    unsubscribe = qc.getQueryCache().subscribe((event) => {
      if (event.type !== "updated" || event.action.type !== "success" || event.action.manual) return;
      const [source, command, cwd] = event.query.queryKey;
      if (source !== "issueStatuses" || cwd !== config.paths.mainClone || JSON.stringify(command) !== JSON.stringify(config.issueTracker?.statusCommand ?? null)) return;
      const statuses = event.query.state.data as IssueStatuses | undefined;
      if (statuses?.[id] === status) resume(Effect.void);
    });
    // Subscribe before invalidation: an immediately resolved reader still
    // reaches the guard, including completion during registry.start().
    void qc.invalidateQueries({ queryKey: ["issueStatuses"] });
    return Effect.sync(unsubscribe);
  });
  yield* Effect.raceFirst(caughtUp, Effect.sleep(Duration.millis(settleMs)));
}, Effect.scoped);
