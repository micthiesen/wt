import { expect, test } from "bun:test";

import type { AiSummary } from "../core/ai.ts";
import { createWtQueryClient } from "./client.ts";
import type { AsyncStorageDb } from "./persister.ts";
import { qk } from "./keys.ts";

function memoryStorage(entries: Promise<Array<[string, string]>>, set = (_key: string, _value: string) => {}): AsyncStorageDb {
  return {
    getItem: async () => null,
    setItem: async (key, value) => { set(key, value); },
    removeItem: async () => {},
    entries: () => entries,
    close: async () => {},
  };
}

async function persistedSnapshot(): Promise<Array<[string, string]>> {
  const entries: Array<[string, string]> = [];
  const written = Promise.withResolvers<void>();
  const source = createWtQueryClient(memoryStorage(Promise.resolve([]), (key, value) => {
    entries.push([key, value]);
    if (entries.length === 3) written.resolve();
  }));
  try {
    await source.restored;
    for (const key of ["orphan", "live", "normal"]) {
      await source.client.fetchQuery({ queryKey: [key], queryFn: async () => "old" });
    }
    await written.promise;
    return entries;
  } finally { await source.shutdown(); }
}

test("late cache restoration cannot resurrect evicted keys or overwrite live data", async () => {
  const entries = await persistedSnapshot();
  const pending = Promise.withResolvers<Array<[string, string]>>();
  const target = createWtQueryClient(memoryStorage(pending.promise));
  try {
    target.evict(["orphan"]);
    target.client.setQueryData(["live"], "fresh");
    pending.resolve(entries);
    await target.restored;
    expect(target.client.getQueryData(["orphan"])).toBeUndefined();
    expect(target.client.getQueryData<string>(["live"])).toBe("fresh");
    expect(target.client.getQueryData<string>(["normal"])).toBe("old");
  } finally { await target.shutdown(); }
});

test("a cache read finishing after shutdown does not repopulate the client", async () => {
  const entries = await persistedSnapshot();
  const pending = Promise.withResolvers<Array<[string, string]>>();
  const target = createWtQueryClient(memoryStorage(pending.promise));
  await target.shutdown();
  pending.resolve(entries);
  await target.restored;
  expect(target.client.getQueryCache().getAll()).toHaveLength(0);
});

test("canonical wtstate never restores or persists a stale title-lock snapshot", async () => {
  const entries = await persistedSnapshot();
  const legacy = JSON.parse(entries[0]![1]);
  legacy.queryKey = ["wtState"];
  legacy.queryHash = JSON.stringify(legacy.queryKey);
  legacy.state.data = { slugs: { task: { section: null, order: 0 } } };
  const legacyEntry: [string, string] = [`wt-${legacy.queryHash}`, JSON.stringify(legacy)];
  const writes: string[] = [];
  const storage = memoryStorage(Promise.resolve([legacyEntry]), (key) => { writes.push(key); });
  storage.getItem = async () => legacyEntry[1];
  const target = createWtQueryClient(storage);
  try {
    await target.restored;
    expect(target.client.getQueryData(["wtState"])).toBeUndefined();
    const canonical = { slugs: { task: { section: null, order: 0, manualTitle: "Pinned" } } };
    let reads = 0;
    expect(await target.client.fetchQuery<typeof canonical>({
      queryKey: ["wtState"],
      queryFn: async () => { reads++; return canonical; },
    })).toEqual(canonical);
    expect(reads).toBe(1);
    expect(writes).toEqual([]);
  } finally { await target.shutdown(); }
});

test("explicit refresh bypasses cold restoration and persists the fresh result", async () => {
  const entries = await persistedSnapshot();
  const entry = entries.find(([, value]) => JSON.parse(value).queryKey[0] === "normal")!;
  const written = Promise.withResolvers<string>();
  const storage = memoryStorage(Promise.resolve([]), (_key, value) => { written.resolve(value); });
  storage.getItem = async () => entry[1];
  const target = createWtQueryClient(storage);
  try {
    await target.restored;
    let calls = 0;
    expect(await target.client.fetchQuery<string>({
      queryKey: ["normal"], staleTime: 0, meta: { forceFresh: true },
      queryFn: async () => { calls++; return "fresh"; },
    })).toBe("fresh");
    expect(calls).toBe(1);
    const persisted = JSON.parse(await written.promise);
    expect(persisted.state.data).toBe("fresh");
    expect(persisted.buster).toBe("v33");
  } finally { await target.shutdown(); }
});

function legacySummaryEntry(
  template: string,
  queryKey: readonly unknown[],
  updatedAt: number,
  title: string | null = "Keep this existing title",
): [string, string] {
  const entry = JSON.parse(template);
  entry.buster = "v32";
  entry.queryKey = queryKey;
  entry.queryHash = JSON.stringify(queryKey);
  entry.state.data = { title, brief: "Different old brief", description: "Existing description." };
  entry.state.dataUpdatedAt = updatedAt;
  entry.state.errorUpdatedAt = updatedAt - 10;
  return [`wt-${entry.queryHash}`, JSON.stringify(entry)];
}

function readableStorage(entry: [string, string], prewarm: boolean) {
  const rows = new Map([entry]);
  const writes: Array<[string, string]> = [];
  const removed: string[] = [];
  const storage = memoryStorage(Promise.resolve(prewarm ? [entry] : []), (key, value) => {
    rows.set(key, value);
    writes.push([key, value]);
  });
  storage.getItem = async (key) => rows.get(key) ?? null;
  storage.removeItem = async (key) => { removed.push(key); rows.delete(key); };
  return { storage, writes, removed };
}

for (const [name, queryKey, title] of [
  ["automatic hash", qk.aiSummary("existing-diff-hash"), "Keep this existing title"],
  ["manual-only slug", qk.wt("existing-worktree").manualSummary(), "Existing manual-only title"],
  ["nullable title", qk.aiSummary("description-only-diff-hash"), null],
] as const) {
  test(`v32 ${name} summary prewarms with its original title and age`, async () => {
    const template = (await persistedSnapshot())[0]![1];
    const updatedAt = Date.now() - 60_000;
    const entry = legacySummaryEntry(template, queryKey, updatedAt, title);
    const { storage, writes, removed } = readableStorage(entry, true);
    const target = createWtQueryClient(storage);
    try {
      await target.restored;
      expect(target.client.getQueryData<AiSummary>(queryKey)).toEqual({ title, description: "Existing description." });
      expect(target.client.getQueryState(queryKey)?.dataUpdatedAt).toBe(updatedAt);
      let calls = 0;
      await target.client.fetchQuery<AiSummary>({
        queryKey, staleTime: Infinity,
        queryFn: async () => { calls++; throw new Error("must not regenerate"); },
      });
      expect(calls).toBe(0);
      expect(writes).toEqual([]);
      expect(removed).toEqual([]);
    } finally { await target.shutdown(); }
  });

  test(`v32 ${name} summary restores on a cold read without calling the model`, async () => {
    const template = (await persistedSnapshot())[0]![1];
    const updatedAt = Date.now() - 60_000;
    const entry = legacySummaryEntry(template, queryKey, updatedAt, title);
    const { storage, writes, removed } = readableStorage(entry, false);
    const target = createWtQueryClient(storage);
    try {
      await target.restored;
      expect(target.client.getQueryData(queryKey)).toBeUndefined();
      // TanStack reinstates the persisted timestamps in its scheduled callback.
      const timestampRestored = Promise.withResolvers<void>();
      const unsubscribe = target.client.getQueryCache().subscribe(({ query }) => {
        if (query.state.dataUpdatedAt === updatedAt) timestampRestored.resolve();
      });
      let calls = 0;
      expect(await target.client.fetchQuery<AiSummary>({
        queryKey, staleTime: Infinity,
        queryFn: async () => { calls++; throw new Error("must not regenerate"); },
      })).toEqual({ title, description: "Existing description." });
      await timestampRestored.promise;
      unsubscribe();
      expect(target.client.getQueryState(queryKey)?.errorUpdatedAt).toBe(updatedAt - 10);
      expect(calls).toBe(0);
      expect(writes).toEqual([]);
      expect(removed).toEqual([]);
    } finally { await target.shutdown(); }
  });
}

test("v32 summary compatibility does not promote malformed, unrelated, old, or expired entries", async () => {
  const template = (await persistedSnapshot())[0]![1];
  const queryKey = qk.wt("manual-worktree").manualSummary();
  const baseline = legacySummaryEntry(template, queryKey, Date.now() - 60_000);
  const changed = (edit: (entry: any) => void): [string, string] => {
    const entry = JSON.parse(baseline[1]);
    edit(entry);
    return [baseline[0], JSON.stringify(entry)];
  };
  const cases: Array<[string, string]> = [
    [baseline[0], "invalid JSON"],
    [baseline[0], "null"],
    changed((entry) => { delete entry.state; }),
    changed((entry) => { entry.state.data.title = 42; }),
    changed((entry) => { entry.state.data.description = null; }),
    changed((entry) => { delete entry.state.data.brief; }),
    changed((entry) => { entry.state.dataUpdatedAt = "invalid timestamp"; }),
    changed((entry) => { entry.buster = "v31"; }),
    changed((entry) => { entry.buster = "v34"; }),
    changed((entry) => { entry.queryHash = "different-identity"; }),
    changed((entry) => { entry.queryKey.push("extra-key-part"); }),
    legacySummaryEntry(template, qk.wt("manual-worktree").firstCommit(), Date.now() - 60_000),
    legacySummaryEntry(template, queryKey, Date.now() - 31 * 24 * 60 * 60 * 1000),
  ];
  for (const entry of cases) {
    for (const prewarm of [true, false]) {
      const { storage, removed } = readableStorage(entry, prewarm);
      const target = createWtQueryClient(storage);
      try {
        await target.restored;
        expect(target.client.getQueryCache().getAll()).toHaveLength(0);
        const key = JSON.parse(entry[0].slice("wt-".length));
        let calls = 0;
        expect(await target.client.fetchQuery<string>({
          queryKey: key, staleTime: Infinity,
          queryFn: async () => { calls++; return "fresh"; },
        })).toBe("fresh");
        expect(calls).toBe(1);
        expect(removed).toContain(entry[0]);
      } finally { await target.shutdown(); }
    }
  }
});
