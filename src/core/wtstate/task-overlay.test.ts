/**
 * Coverage for the task-inbox overlay fields (`taskPinned`,
 * `taskSnoozedBucket`) added to `WtSlugState`.
 *
 * `readWtState` hardcodes `STATE_FILE` to `~/.cache/wt/state.json` with
 * no injection seam (no `WT_STATE_DIR` env override, and the writers'
 * `withWtStateLock` pulls in `core/config.ts`, which fails fast without
 * a real `config.toml` — see CLAUDE.md's "No client-app defaults in
 * code"). So these tests exercise the pure parse/validation path
 * (`parseWtState`, exported from `wtstate/io.ts` for exactly this
 * reason) via JSON round-trips instead of the writers or the real state
 * file. `setTaskPinned`/`setTaskSnooze` in `wtstate/sections.ts` are
 * thin `withWtStateLock` wrappers mirroring `setSlugBase` /
 * `toggleSlugAutomationsPaused` — reviewed by inspection, not run here.
 */
import { expect, test } from "bun:test";

import { parseWtState } from "./io.ts";
import type { WtState } from "./types.ts";

function baseState(slugs: Record<string, Record<string, unknown>>): unknown {
  return {
    slugs,
    sectionsOrder: ["\0inbox"],
    foldedSections: [],
    pausedStacks: [],
    automationsPaused: false,
    removed: [],
  };
}

/** Round-trip through JSON, the same encoding `writeWtState`/`readWtState` use. */
function roundTrip(data: unknown): WtState {
  return parseWtState(JSON.parse(JSON.stringify(data)));
}

test("taskPinned: true survives the round trip", () => {
  const state = roundTrip(baseState({ a: { section: null, order: 0, taskPinned: true } }));
  expect(state.slugs.a?.taskPinned).toBe(true);
});

test("taskPinned: absent field stays absent", () => {
  const state = roundTrip(baseState({ a: { section: null, order: 0 } }));
  expect(state.slugs.a?.taskPinned).toBeUndefined();
});

test("taskPinned: only strictly `true` is accepted — false/truthy junk is dropped", () => {
  for (const junk of [false, "true", 1, 0, null]) {
    const state = roundTrip(baseState({ a: { section: null, order: 0, taskPinned: junk } }));
    expect(state.slugs.a?.taskPinned).toBeUndefined();
  }
});

test("taskSnoozedBucket: a non-empty string survives the round trip", () => {
  const state = roundTrip(
    baseState({ a: { section: null, order: 0, taskSnoozedBucket: "later" } }),
  );
  expect(state.slugs.a?.taskSnoozedBucket).toBe("later");
});

test("taskSnoozedBucket: absent field stays absent", () => {
  const state = roundTrip(baseState({ a: { section: null, order: 0 } }));
  expect(state.slugs.a?.taskSnoozedBucket).toBeUndefined();
});

test("taskSnoozedBucket: empty/whitespace/non-string values are dropped", () => {
  for (const junk of ["", "   ", 42, null, true]) {
    const state = roundTrip(
      baseState({ a: { section: null, order: 0, taskSnoozedBucket: junk } }),
    );
    expect(state.slugs.a?.taskSnoozedBucket).toBeUndefined();
  }
});

test("both fields coexist on one slug, alongside pre-existing fields", () => {
  const state = roundTrip(
    baseState({
      a: {
        section: "Backend",
        order: 3,
        baseBranch: "main",
        automationsPaused: true,
        taskPinned: true,
        taskSnoozedBucket: "this-week",
      },
    }),
  );
  expect(state.slugs.a).toEqual({
    section: "Backend",
    order: 3,
    baseBranch: "main",
    automationsPaused: true,
    taskPinned: true,
    taskSnoozedBucket: "this-week",
  });
});

test("clearing: a record written without the fields (simulating a delete) round-trips clean", () => {
  // Simulates what `setTaskPinned(slug, false)` / `setTaskSnooze(slug,
  // null)` produce on disk: the keys are deleted from the record
  // entirely, not written as `false`/`null`.
  const pinned = roundTrip(baseState({ a: { section: null, order: 0, taskPinned: true } }));
  const cleared = roundTrip(baseState({ a: { section: pinned.slugs.a!.section, order: 0 } }));
  expect(cleared.slugs.a?.taskPinned).toBeUndefined();
  expect(cleared.slugs.a?.taskSnoozedBucket).toBeUndefined();
});
