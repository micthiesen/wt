import { mkdtempSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { expect, test } from "bun:test";

import { createTaskFocusStore } from "./task-focus.ts";

function tempPath(): string {
  const dir = mkdtempSync(join(tmpdir(), "wt-task-focus-"));
  return join(dir, "task-focus.json");
}

test("record stamps a slug and getSnapshot reflects it", () => {
  const store = createTaskFocusStore(tempPath());
  expect(store.getSnapshot().has("my-slug")).toBe(false);

  const now = Date.now();
  store.record("my-slug", now);

  expect(store.getSnapshot().get("my-slug")).toBe(now);
});

test("a stamp persists across a second store instance on the same file", () => {
  const filePath = tempPath();
  const now = Date.now();
  const first = createTaskFocusStore(filePath);
  first.record("slug-a", now);

  const second = createTaskFocusStore(filePath);
  expect(second.getSnapshot().get("slug-a")).toBe(now);
});

test("a missing file starts empty rather than throwing", () => {
  const store = createTaskFocusStore(join(tmpdir(), "wt-task-focus-does-not-exist", "nope.json"));
  expect(store.getSnapshot().size).toBe(0);
});

test("a corrupt file is tolerated and treated as empty", () => {
  const filePath = tempPath();
  writeFileSync(filePath, "{ not valid json");

  const store = createTaskFocusStore(filePath);

  expect(store.getSnapshot().size).toBe(0);
  // And it's still writable afterward.
  store.record("slug-b", 7);
  expect(store.getSnapshot().get("slug-b")).toBe(7);
});

test("a non-object JSON value is tolerated and treated as empty", () => {
  const filePath = tempPath();
  writeFileSync(filePath, "[1,2,3]");

  const store = createTaskFocusStore(filePath);

  expect(store.getSnapshot().size).toBe(0);
});

test("entries older than 30 days are pruned on load", () => {
  const filePath = tempPath();
  const now = Date.now();
  const THIRTY_ONE_DAYS_MS = 31 * 24 * 60 * 60 * 1000;
  writeFileSync(
    filePath,
    JSON.stringify({
      stale: now - THIRTY_ONE_DAYS_MS,
      fresh: now - 1_000,
    }),
  );

  const store = createTaskFocusStore(filePath);
  const snapshot = store.getSnapshot();

  expect(snapshot.has("stale")).toBe(false);
  expect(snapshot.get("fresh")).toBe(now - 1_000);
});

test("snapshot identity is stable across reads and changes only after record", () => {
  const store = createTaskFocusStore(tempPath());

  const a = store.getSnapshot();
  const b = store.getSnapshot();
  expect(a).toBe(b);

  store.record("slug-c", 5);
  const c = store.getSnapshot();
  expect(c).not.toBe(b);

  const d = store.getSnapshot();
  expect(d).toBe(c);
});

test("subscribe fires listeners on record and unsubscribe stops delivery", () => {
  const store = createTaskFocusStore(tempPath());
  let calls = 0;
  const unsubscribe = store.subscribe(() => {
    calls++;
  });

  store.record("slug-d", 1);
  expect(calls).toBe(1);

  unsubscribe();
  store.record("slug-e", 2);
  expect(calls).toBe(1);
});
