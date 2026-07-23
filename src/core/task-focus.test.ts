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

test("a stamp persists across a second store instance on the same file, once loaded", () => {
  const filePath = tempPath();
  const now = Date.now();
  const first = createTaskFocusStore(filePath);
  first.record("slug-a", now);

  // getSnapshot() never loads on its own (see the type doc) — a fresh
  // store instance has to call load() explicitly to pick up what's on
  // disk, the same way the hub pane does at startup.
  const second = createTaskFocusStore(filePath);
  expect(second.getSnapshot().has("slug-a")).toBe(false);
  second.load();
  expect(second.getSnapshot().get("slug-a")).toBe(now);
});

test("a missing file loads as empty rather than throwing", () => {
  const store = createTaskFocusStore(join(tmpdir(), "wt-task-focus-does-not-exist", "nope.json"));
  store.load();
  expect(store.getSnapshot().size).toBe(0);
});

test("a corrupt file is tolerated and treated as empty", () => {
  const filePath = tempPath();
  writeFileSync(filePath, "{ not valid json");

  const store = createTaskFocusStore(filePath);
  store.load();

  expect(store.getSnapshot().size).toBe(0);
  // And it's still writable afterward.
  store.record("slug-b", 7);
  expect(store.getSnapshot().get("slug-b")).toBe(7);
});

test("a non-object JSON value is tolerated and treated as empty", () => {
  const filePath = tempPath();
  writeFileSync(filePath, "[1,2,3]");

  const store = createTaskFocusStore(filePath);
  store.load();

  expect(store.getSnapshot().size).toBe(0);
});

test("mixed-validity entries: only finite-number values survive load", () => {
  const filePath = tempPath();
  // "a"'s value must be a recent-enough epoch-ms or the 30-day prune
  // (a separate, unrelated guard) would drop it too and defeat the
  // point of this test.
  const a = Date.now() - 1_000;
  writeFileSync(filePath, JSON.stringify({ a, b: "x", c: null }));

  const store = createTaskFocusStore(filePath);
  store.load();
  const snapshot = store.getSnapshot();

  expect(snapshot.get("a")).toBe(a);
  expect(snapshot.has("b")).toBe(false);
  expect(snapshot.has("c")).toBe(false);
  expect(snapshot.size).toBe(1);
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
  store.load();
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

test("rapid re-stamps within 1500ms of the existing stamp are gated to a single write", () => {
  const filePath = tempPath();
  const store = createTaskFocusStore(filePath);
  // A recent-enough epoch-ms so the unrelated 30-day prune-on-load
  // doesn't drop it out from under this test.
  const t0 = Date.now() - 10_000;

  store.record("slug-f", t0);
  store.record("slug-f", t0 + 500); // within the 1500ms gate — no-op
  store.record("slug-f", t0 + 1000); // still within the gate — no-op

  // In-memory value stays at the first stamp; the gated calls never
  // touched the map or bumped the snapshot.
  expect(store.getSnapshot().get("slug-f")).toBe(t0);

  // A second store instance reading the file confirms only the first
  // record() actually wrote — the gated calls never hit disk either.
  const second = createTaskFocusStore(filePath);
  second.load();
  expect(second.getSnapshot().get("slug-f")).toBe(t0);

  // Once nowMs clears the gate window (>= 1500ms after the existing
  // stamp), the next record() persists normally.
  store.record("slug-f", t0 + 1500);
  expect(store.getSnapshot().get("slug-f")).toBe(t0 + 1500);
});
