import { describe, expect, test } from "bun:test";
import { QueryClient } from "@tanstack/react-query";

import { runOptimisticMutation } from "./hooks.ts";

/**
 * Behavioral pins for `runOptimisticMutation` — the TanStack-scoped
 * replacement for the old hand-rolled mutation chain. These encode the
 * three properties the TUI's optimistic mutations depend on:
 *
 *  1. Same-filter calls run serialized, in submission order (scope.id).
 *  2. A failed mutation rolls back to the PRE-mutation snapshot, and a
 *     queued second mutation snapshots AFTER the first settles — so a
 *     rollback can never resurrect another call's optimistic state.
 *  3. A background refetch that lands mid-mutation cannot clobber the
 *     optimistic patch (the cache-subscription guard re-applies it).
 */

type Data = { v: string };

function deferred(): { promise: Promise<void>; resolve: () => void; reject: (e: Error) => void } {
  let resolve!: () => void;
  let reject!: (e: Error) => void;
  const promise = new Promise<void>((res, rej) => {
    resolve = res;
    reject = rej;
  });
  return { promise, resolve, reject };
}

const tick = () => new Promise<void>((r) => setTimeout(r, 0));

describe("runOptimisticMutation", () => {
  test("applies patch optimistically and settles", async () => {
    const qc = new QueryClient();
    qc.setQueryData<Data>(["github", "x"], { v: "server" });
    const gate = deferred();
    const done = runOptimisticMutation<Data>(qc, {
      filter: { queryKey: ["github"] },
      patch: (prev) => (prev ? { v: "optimistic" } : prev),
      run: () => gate.promise,
    });
    await tick();
    expect(qc.getQueryData<Data>(["github", "x"])?.v).toBe("optimistic");
    gate.resolve();
    await done;
    expect(qc.getQueryData<Data>(["github", "x"])?.v).toBe("optimistic");
  });

  test("serializes same-filter calls in submission order", async () => {
    const qc = new QueryClient();
    qc.setQueryData<Data>(["github", "x"], { v: "server" });
    const order: string[] = [];
    const gateA = deferred();
    const a = runOptimisticMutation<Data>(qc, {
      filter: { queryKey: ["github"] },
      patch: (prev) => prev,
      run: async () => {
        order.push("a-start");
        await gateA.promise;
        order.push("a-end");
      },
    });
    const b = runOptimisticMutation<Data>(qc, {
      filter: { queryKey: ["github"] },
      patch: (prev) => prev,
      run: async () => {
        order.push("b-start");
      },
    });
    await tick();
    expect(order).toEqual(["a-start"]); // b queued behind a's scope
    gateA.resolve();
    await Promise.all([a, b]);
    expect(order).toEqual(["a-start", "a-end", "b-start"]);
  });

  test("failed call rolls back; queued call snapshots post-settle state", async () => {
    const qc = new QueryClient();
    qc.setQueryData<Data>(["github", "x"], { v: "server" });
    const gateA = deferred();
    const a = runOptimisticMutation<Data>(qc, {
      filter: { queryKey: ["github"] },
      patch: () => ({ v: "a-optimistic" }),
      run: () => gateA.promise,
    });
    // B queues behind A while A's optimistic patch is visible.
    const gateB = deferred();
    const b = runOptimisticMutation<Data>(qc, {
      filter: { queryKey: ["github"] },
      patch: () => ({ v: "b-optimistic" }),
      run: () => gateB.promise,
    });
    await tick();
    expect(qc.getQueryData<Data>(["github", "x"])?.v).toBe("a-optimistic");
    // A fails → rolls back to "server" BEFORE b snapshots/patches.
    gateA.reject(new Error("a failed"));
    await expect(a).rejects.toThrow("a failed");
    await tick();
    expect(qc.getQueryData<Data>(["github", "x"])?.v).toBe("b-optimistic");
    // B fails too → must restore "server", not a's optimistic value.
    gateB.reject(new Error("b failed"));
    await expect(b).rejects.toThrow("b failed");
    expect(qc.getQueryData<Data>(["github", "x"])?.v).toBe("server");
  });

  test("mid-flight background refetch cannot clobber the patch", async () => {
    const qc = new QueryClient();
    qc.setQueryData<Data>(["github", "x"], { v: "server" });
    const gate = deferred();
    const done = runOptimisticMutation<Data>(qc, {
      filter: { queryKey: ["github"] },
      patch: (prev) => (prev ? { v: "optimistic" } : prev),
      run: () => gate.promise,
    });
    await tick();
    // A background refetch starts DURING the mutation (after
    // cancelQueries) and resolves with pre-mutation server truth.
    await qc.fetchQuery<Data>({
      queryKey: ["github", "x"],
      queryFn: async () => ({ v: "stale-refetch" }),
      staleTime: 0,
    });
    await tick();
    // Guard re-applied the patch on top of the refetched data.
    expect(qc.getQueryData<Data>(["github", "x"])?.v).toBe("optimistic");
    gate.resolve();
    await done;
    // After settle the guard is gone — a fresh fetch wins again.
    await qc.refetchQueries({ queryKey: ["github", "x"] });
  });

  test("non-matching entries are untouched by patch and guard", async () => {
    const qc = new QueryClient();
    qc.setQueryData<Data>(["github", "x"], { v: "server" });
    qc.setQueryData<Data>(["other"], { v: "other" });
    const gate = deferred();
    const done = runOptimisticMutation<Data>(qc, {
      filter: { queryKey: ["github"] },
      patch: () => ({ v: "optimistic" }),
      run: () => gate.promise,
    });
    await tick();
    expect(qc.getQueryData<Data>(["other"])?.v).toBe("other");
    gate.resolve();
    await done;
    expect(qc.getQueryData<Data>(["other"])?.v).toBe("other");
  });
});
