/**
 * The archived-row automations pause, round-tripped through a real
 * `state.json`.
 *
 * The bug this pins is invisible in memory: `reapWtState` drops the
 * per-slug record with the worktree, while a post-merge `external`
 * automation deliberately outlives both — so a pause set before the
 * merge was gone seconds after it, and the run it was set to stop fired
 * anyway. Only reading the row back after a reap shows that.
 *
 * Subprocess + generated `WT_CONFIG`, since config loads once at module
 * init and `cache_db` (via `cache_root`) is what isolates the state file.
 */
import { afterAll, expect, test } from "bun:test";

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const dirs: string[] = [];
afterAll(() => {
  for (const d of dirs) rmSync(d, { recursive: true, force: true });
});

const WTSTATE_MOD = JSON.stringify(
  pathToFileURL(join(import.meta.dir, "..", "wtstate.ts")).href,
);

function inSandbox(script: string): string {
  const root = mkdtempSync(join(tmpdir(), "wt-rap-"));
  dirs.push(root);
  const cfg = join(root, "config.toml");
  writeFileSync(
    cfg,
    `
[paths]
main_clone = ${JSON.stringify(join(root, "main"))}
worktree_root = ${JSON.stringify(join(root, "wts"))}
cache_db = ${JSON.stringify(join(root, "cache", "cache.sqlite"))}
state_db = ${JSON.stringify(join(root, "state", "wt.sqlite"))}

[branch]
prefix = "t"
`,
  );
  const r = Bun.spawnSync([process.execPath, "-e", script], {
    cwd: root,
    env: { ...process.env, WT_CONFIG: cfg },
    stdout: "pipe",
    stderr: "pipe",
  });
  expect(r.exitCode, r.stderr.toString()).toBe(0);
  return r.stdout.toString();
}

test("a pause set while live survives into the removed history", () => {
  const out = inSandbox(`
    const m = await import(${WTSTATE_MOD});
    m.toggleSlugAutomationsPaused("s");
    m.recordRemovedWorktrees([
      { slug: "s", branch: "t/s", removedAt: new Date().toISOString() },
    ]);
    const entry = m.readWtState().removed.find((e) => e.slug === "s");
    console.log(JSON.stringify({ paused: entry.automationsPaused === true }));
  `);
  expect(JSON.parse(out.trim())).toEqual({ paused: true });
});

test("an unpaused worktree records no flag — absence means not paused", () => {
  const out = inSandbox(`
    const m = await import(${WTSTATE_MOD});
    m.recordRemovedWorktrees([
      { slug: "s", branch: "t/s", removedAt: new Date().toISOString() },
    ]);
    const entry = m.readWtState().removed.find((e) => e.slug === "s");
    console.log(JSON.stringify({ flag: entry.automationsPaused ?? null }));
  `);
  expect(JSON.parse(out.trim())).toEqual({ flag: null });
});

test("toggling on the archived row persists, and toggles back off", () => {
  const out = inSandbox(`
    const m = await import(${WTSTATE_MOD});
    m.recordRemovedWorktrees([
      { slug: "s", branch: "t/s", removedAt: new Date().toISOString() },
    ]);
    const on = m.toggleRemovedAutomationsPaused("s");
    const afterOn = m.readWtState().removed.find((e) => e.slug === "s").automationsPaused;
    const off = m.toggleRemovedAutomationsPaused("s");
    const afterOff = m.readWtState().removed.find((e) => e.slug === "s").automationsPaused;
    console.log(JSON.stringify({ on, afterOn, off, afterOff: afterOff ?? null }));
  `);
  expect(JSON.parse(out.trim())).toEqual({
    on: true,
    afterOn: true,
    off: false,
    afterOff: null,
  });
});

test("a later minimal confirm does not blank a pause toggled on the archived row", () => {
  // `removeWorktree` re-records the slug after the reap with only the
  // bare fields. That write must compose, not clobber — the same
  // ordering hazard `work` has.
  const out = inSandbox(`
    const m = await import(${WTSTATE_MOD});
    const removedAt = Date.now();
    m.recordRemovedWorktrees([
      { slug: "s", branch: "t/s", removedAt: new Date(removedAt).toISOString() },
    ]);
    m.toggleRemovedAutomationsPaused("s");
    m.recordRemovedWorktrees([
      { slug: "s", branch: "t/s", removedAt: new Date(removedAt + 5_000).toISOString() },
    ]);
    const entry = m.readWtState().removed.find((e) => e.slug === "s");
    console.log(JSON.stringify({ paused: entry.automationsPaused === true }));
  `);
  expect(JSON.parse(out.trim())).toEqual({ paused: true });
});

test("a later minimal confirm preserves the archived release evidence", () => {
  const out = inSandbox(`
    const m = await import(${WTSTATE_MOD});
    const removedAt = Date.now();
    m.recordRemovedWorktrees([{ slug: "s", branch: "t/s",
      removedAt: new Date(removedAt).toISOString(),
      prState: "MERGED", landedOnAtRemoval: "base", prMergeCommitOid: "merge-sha" }]);
    m.recordRemovedWorktrees([{ slug: "s", branch: "t/s",
      removedAt: new Date(removedAt + 5_000).toISOString() }]);
    const entry = m.readWtState().removed.find((e) => e.slug === "s");
    console.log(JSON.stringify({ landing: entry.landedOnAtRemoval, sha: entry.prMergeCommitOid }));
  `);
  expect(JSON.parse(out.trim())).toEqual({ landing: "base", sha: "merge-sha" });
});

test("toggling a slug that is not in the history reports null", () => {
  const out = inSandbox(`
    const m = await import(${WTSTATE_MOD});
    console.log(JSON.stringify({ r: m.toggleRemovedAutomationsPaused("nope") }));
  `);
  expect(JSON.parse(out.trim())).toEqual({ r: null });
});
