import { expect, test } from "bun:test";
import { QueryClient } from "@tanstack/react-query";
import { Effect, Fiber } from "effect";
import { TestClock } from "effect/testing";
import type { ActionRun, ActionStatus } from "../core/actions.ts";
import { config } from "../core/config.ts";
import { issueStatusExpectations, trackIssueStatusAction } from "./issue-status.ts";
import { issueStatusesQuery, ISSUE_STATUS_POLL_MS } from "./queries/issue-status.ts";
import { qk } from "./keys.ts";

function fixture() {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const runs = new Map<string, ActionRun>();
  const listeners = new Set<() => void>();
  const registry = {
    getSnapshot: () => runs,
    subscribe(listener: () => void) { listeners.add(listener); return () => { listeners.delete(listener); }; },
  };
  function start(slug = "example"): ActionRun {
    const run: ActionRun = { slug, kind: "shell", actionId: "move", actionName: "Move", prompt: "", startedAt: 1, status: "running", lines: [], runDir: `/tmp/issue-test-${slug}`, affects: ["issue"] };
    runs.set(slug, run);
    return run;
  }
  function finish(run: ActionRun, status: ActionStatus) {
    runs.set(run.slug, { ...run, status, endedAt: 2 });
    for (const listener of listeners) listener();
  }
  const store = issueStatusExpectations(qc);
  return { qc, registry, start, finish, store, listeners };
}

test("reader key includes provider, cwd and sorted issue identity; absent reader is disabled", () => {
  const q = issueStatusesQuery(["ENG-2", "ENG-1"], ["tracker", "{ids}"], "/main");
  expect([...q.queryKey]).toEqual(["issueStatuses", ["tracker", "{ids}"], "/main", ["ENG-1", "ENG-2"]]);
  expect(q.refetchInterval).toBe(ISSUE_STATUS_POLL_MS);
  expect(issueStatusesQuery(["ENG-1"], null).enabled).toBe(false);
  expect(issueStatusesQuery([], ["tracker", "{ids}"]).enabled).toBe(false);
});

test("a reader failure preserves the last complete batch in the query cache", async () => {
  const qc = new QueryClient({ defaultOptions: { queries: { retry: false } } });
  const query = issueStatusesQuery(["ENG-1"], ["bun", "-e", 'console.error("denied");process.exit(2)', "{ids}"], "/tmp");
  qc.setQueryData(query.queryKey, { "ENG-1": "Open" });
  await expect(qc.fetchQuery({ ...query, staleTime: 0 })).rejects.toThrow("denied");
  expect(qc.getQueryData<Record<string, string>>(query.queryKey)).toEqual({ "ENG-1": "Open" });
  qc.clear();
});

test("failed action removes only its own expectation without changing server truth", async () => {
  const f = fixture();
  const key = qk.issueStatuses(["ENG-1", "ENG-2"], null, "/main");
  f.qc.setQueryData(key, { "ENG-1": "Open", "ENG-2": "Backlog" });
  await Effect.runPromise(Effect.scoped(Effect.gen(function* () {
    const first = f.start("first");
    const second = f.start("second");
    const one = yield* trackIssueStatusAction(f.qc, first, "ENG-1", "Review", f.registry).pipe(Effect.forkChild);
    const two = yield* trackIssueStatusAction(f.qc, second, "ENG-2", "Ready", f.registry).pipe(Effect.forkChild);
    yield* Effect.yieldNow;
    expect(f.store.getSnapshot().get("ENG-1")?.status).toBe("Review");
    expect(f.store.getSnapshot().get("ENG-2")?.status).toBe("Ready");
    f.finish(first, "failed");
    yield* Fiber.join(one);
    expect(f.store.getSnapshot().has("ENG-1")).toBe(false);
    expect(f.store.getSnapshot().get("ENG-2")?.status).toBe("Ready");
    expect(f.qc.getQueryData<Record<string, string>>(key)).toEqual({ "ENG-1": "Open", "ENG-2": "Backlog" });
    f.finish(second, "killed");
    yield* Fiber.join(two);
    expect(f.store.getSnapshot().size).toBe(0);
    expect(f.listeners.size).toBe(0);
  })));
});

test("an older same-issue failure cannot roll back a newer action", async () => {
  const f = fixture();
  await Effect.runPromise(Effect.scoped(Effect.gen(function* () {
    const first = f.start("first");
    const second = f.start("second");
    const one = yield* trackIssueStatusAction(f.qc, first, "ENG-1", "Review", f.registry).pipe(Effect.forkChild);
    yield* Effect.yieldNow;
    const two = yield* trackIssueStatusAction(f.qc, second, "ENG-1", "Ready", f.registry).pipe(Effect.forkChild);
    yield* Effect.yieldNow;
    f.finish(first, "failed");
    yield* Fiber.join(one);
    expect(f.store.getSnapshot().get("ENG-1")?.status).toBe("Ready");
    f.finish(second, "failed");
    yield* Fiber.join(two);
    expect(f.store.getSnapshot().size).toBe(0);
  })));
});

test("success holds through stale refetch, then releases on live catch-up", async () => {
  const f = fixture();
  const key = qk.issueStatuses(["ENG-1"], config.issueTracker?.statusCommand ?? null, config.paths.mainClone);
  await Effect.runPromise(Effect.scoped(Effect.gen(function* () {
    const run = f.start();
    const fiber = yield* trackIssueStatusAction(f.qc, run, "ENG-1", "Review", f.registry).pipe(Effect.forkChild);
    yield* Effect.yieldNow;
    f.finish(run, "succeeded");
    yield* Effect.yieldNow;
    yield* Effect.promise(() => f.qc.fetchQuery({ queryKey: key, queryFn: () => ({ "ENG-1": "Open" }) }));
    expect(f.store.getSnapshot().get("ENG-1")?.status).toBe("Review");
    yield* Effect.promise(() => f.qc.fetchQuery({ queryKey: key, queryFn: () => ({ "ENG-1": "Review" }) }));
    yield* Fiber.join(fiber);
    expect(f.store.getSnapshot().size).toBe(0);
  })));
});

test("unconfirmable success expires on TestClock and a replaced run cannot pin the display", async () => {
  const f = fixture();
  await Effect.runPromise(Effect.scoped(Effect.gen(function* () {
    const run = f.start();
    const fiber = yield* trackIssueStatusAction(f.qc, run, "ENG-1", "Review", f.registry, 12_000).pipe(Effect.forkChild);
    yield* Effect.yieldNow;
    f.finish(run, "succeeded");
    yield* TestClock.adjust(12_000);
    yield* Fiber.join(fiber);
    expect(f.store.getSnapshot().size).toBe(0);
    yield* trackIssueStatusAction(f.qc, { ...run, runDir: "obsolete" }, "ENG-1", "Review", f.registry);
    expect(f.store.getSnapshot().size).toBe(0);
  })).pipe(Effect.provide(TestClock.layer())));
});
