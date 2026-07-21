/**
 * Rift-backend restack: the engine reads refs from each slice's own
 * object store, and under rift each slice is an INDEPENDENT clone. These
 * fixtures model that with genuinely separate clones (no shared db, a
 * `.rift` marker) and pin the two failures the ref-materialization fixes
 * (`stack-ops/rift-refs.ts`): anchoring in Pass 1 and resolving the
 * parent's just-replayed tip in Pass 2. No `rift` binary needed — the
 * engine only cares that the commits live in separate `.git` dirs.
 */
import { afterAll, expect, test } from "bun:test";

import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { resolveAnchor } from "./stack-ops.ts";
import { restackEngine } from "./stack-ops/engine.ts";

const dirs: string[] = [];
afterAll(() => {
  for (const d of dirs) rmSync(d, { recursive: true, force: true });
});

function git(cwd: string, args: string[]): string {
  const r = Bun.spawnSync(["git", ...args], {
    cwd,
    env: {
      ...process.env,
      GIT_CONFIG_GLOBAL: "/dev/null",
      GIT_CONFIG_SYSTEM: "/dev/null",
      GIT_AUTHOR_NAME: "t",
      GIT_AUTHOR_EMAIL: "t@t",
      GIT_COMMITTER_NAME: "t",
      GIT_COMMITTER_EMAIL: "t@t",
    },
  });
  if (r.exitCode !== 0) {
    throw new Error(`git ${args.join(" ")}: ${new TextDecoder().decode(r.stderr)}`);
  }
  return new TextDecoder().decode(r.stdout).trim();
}

function tmp(name: string): string {
  const d = mkdtempSync(join(tmpdir(), name));
  dirs.push(d);
  return d;
}

/**
 * Build a parent clone (main→M0, p→P1) and a SEPARATE child clone that
 * forked off the parent's tip exactly as the create-path does (fetch the
 * parent branch into FETCH_HEAD, `switch -c c`), then commit C. The child
 * has NO local `p` ref and NO shared object db — the rift world.
 */
function buildStack() {
  const parent = tmp("wt-rift-parent-");
  git(parent, ["init", "-q", "-b", "main"]);
  git(parent, ["commit", "-q", "--allow-empty", "-m", "M0"]);
  git(parent, ["checkout", "-q", "-b", "p"]);
  git(parent, ["commit", "-q", "--allow-empty", "-m", "P1"]);
  const p1 = git(parent, ["rev-parse", "HEAD"]);

  const child = tmp("wt-rift-child-");
  git(child, ["init", "-q", "-b", "main"]);
  // Local identity so the engine's internal git calls (which don't carry
  // the test's env) can rebase/commit hermetically.
  git(child, ["config", "user.email", "t@t"]);
  git(child, ["config", "user.name", "t"]);
  git(child, ["fetch", "--no-tags", parent, "refs/heads/p"]);
  git(child, ["switch", "-q", "-c", "c", "FETCH_HEAD"]);
  git(child, ["commit", "-q", "--allow-empty", "-m", "C"]);
  writeFileSync(join(child, ".rift"), ""); // mark as a rift clone
  return { parent, child, p1 };
}

test("rift: Pass-1 anchor needs the parent ref fetched into the child clone", async () => {
  const { parent, child, p1 } = buildStack();

  // Baseline (the reported bug): with no recorded baseSha and the parent
  // ref absent from the child's store, the anchor can't resolve → the run
  // bails during planning. This is exactly the "can't anchor ENG-5316".
  expect(await resolveAnchor({ branch: "c", baseSha: undefined }, "p", child)).toBeNull();

  // A recorded baseSha rescues it (its object was fetched in at create),
  // which is why only record-free / sha-less members hit the bail.
  expect(await resolveAnchor({ branch: "c", baseSha: p1 }, "p", child)).toBe(p1);

  // The fix: materialize the parent ref into the child (what
  // materializeSliceRefsPreAnchor does for an in-chain parent). Now the
  // merge-base resolves the anchor even with no recorded baseSha.
  git(child, ["fetch", "--no-tags", parent, "+refs/heads/p:refs/heads/p"]);
  expect(await resolveAnchor({ branch: "c", baseSha: undefined }, "p", child)).toBe(p1);
});

test("rift: Pass-2 replay needs the parent's new tip fetched into the child", async () => {
  const { parent, child, p1 } = buildStack();

  // The parent replays onto newer trunk in its own clone → new tip p1'.
  git(parent, ["checkout", "-q", "main"]);
  git(parent, ["commit", "-q", "--allow-empty", "-m", "M1"]);
  const m1 = git(parent, ["rev-parse", "HEAD"]);
  git(parent, ["checkout", "-q", "p"]);
  const m0 = git(parent, ["merge-base", "p", "main"]);
  git(parent, ["rebase", "--onto", m1, m0, "p"]);
  const p1b = git(parent, ["rev-parse", "p"]);
  expect(p1b).not.toBe(p1);

  // Baseline: without the parent's new commits, the child can't resolve
  // the base it's told to rebase onto.
  const pre = await restackEngine.replayStep(
    { branch: "c", worktreePath: child, anchor: p1, newBase: p1b },
    () => {},
  );
  expect(pre.ok).toBe(false);
  if (!pre.ok) expect(pre.error).toContain("cannot resolve base ref");

  // The fix: materialize the parent's NEW tip into the child (what
  // materializeParentNewTip does), then the squash-safe replay lands C on
  // p1' with only C's own commit moved.
  git(child, ["fetch", "--no-tags", parent, "+refs/heads/p:refs/heads/p"]);
  const out = await restackEngine.replayStep(
    { branch: "c", worktreePath: child, anchor: p1, newBase: p1b },
    () => {},
  );
  expect(out.ok).toBe(true);
  if (out.ok) {
    expect(out.moved).toBe(true);
    expect(git(child, ["rev-parse", "c~1"])).toBe(p1b); // C now sits on p1'
    expect(git(child, ["log", "-1", "--format=%s", "c"])).toBe("C");
  }
});
