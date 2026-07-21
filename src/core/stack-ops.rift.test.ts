/**
 * Restack across independent clones (the rift backend) and a mix of
 * linked worktrees + rift clones. A rift checkout is a full clone with its
 * own object store, so a sibling slice's branch isn't a LOCAL ref there —
 * it's reachable only through the `origin/<branch>` remote-tracking ref
 * every clone fetches. These fixtures model that with genuinely separate
 * clones sharing one bare origin, and pin the two resolutions that must
 * work cross-clone: the Pass-1 anchor ref and the Pass-2 new base.
 */
import { afterAll, expect, test } from "bun:test";

import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { resolveAnchor } from "./stack-ops.ts";
import { anchorParentRef, resolveNewBaseSha } from "./stack-ops/replay.ts";
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

const step = (parentBranch: string | null, baseSha?: string) => ({
  slug: "s",
  branch: "c",
  parentBranch,
  ...(baseSha ? { baseSha } : {}),
  hasRecord: parentBranch !== null,
  worktreePath: "",
});

/**
 * Bare `origin`, a `parent` clone with branch `p` pushed to it, and a
 * SEPARATE `child` clone that forked `c` off `origin/p`. The child has
 * `refs/remotes/origin/p` but NO local `refs/heads/p` — the rift world.
 */
function buildClones() {
  const origin = tmp("wt-rift-origin-");
  git(origin, ["init", "-q", "--bare", "-b", "main"]);

  const parent = tmp("wt-rift-parent-");
  git(parent, ["clone", "-q", origin, "."]);
  git(parent, ["config", "user.email", "t@t"]);
  git(parent, ["config", "user.name", "t"]);
  git(parent, ["commit", "-q", "--allow-empty", "-m", "M0"]);
  git(parent, ["push", "-q", "origin", "main"]);
  git(parent, ["checkout", "-q", "-b", "p"]);
  git(parent, ["commit", "-q", "--allow-empty", "-m", "P1"]);
  const p1 = git(parent, ["rev-parse", "HEAD"]);
  git(parent, ["push", "-q", "origin", "p"]);

  const child = tmp("wt-rift-child-");
  git(child, ["clone", "-q", origin, "."]);
  git(child, ["config", "user.email", "t@t"]);
  git(child, ["config", "user.name", "t"]);
  git(child, ["checkout", "-q", "-b", "c", "origin/p"]);
  git(child, ["commit", "-q", "--allow-empty", "-m", "C"]);
  return { origin, parent, child, p1 };
}

test("rift: the anchor resolves via origin/<parent>, not the absent local branch", async () => {
  const { child, p1 } = buildClones();

  // The child has no local `p`, so the bare name — what the old engine
  // passed to merge-base — can't anchor (the reported "no merge-base
  // with <parent>").
  expect(await resolveAnchor(step("p"), "p", child)).toBeNull();

  // `anchorParentRef` prefers the local branch, falls back to the
  // remote-tracking ref every clone carries — which resolves here.
  const parentRef = await anchorParentRef(step("p"), "main", child);
  expect(await resolveAnchor(step("p"), parentRef, child)).toBe(p1);
});

test("rift: the new base is brought over from the parent clone when absent locally", async () => {
  const { parent, child } = buildClones();

  // The parent replays onto newer trunk in its OWN clone → new tip p1'.
  git(parent, ["checkout", "-q", "main"]);
  git(parent, ["commit", "-q", "--allow-empty", "-m", "M1"]);
  const m1 = git(parent, ["rev-parse", "HEAD"]);
  git(parent, ["checkout", "-q", "p"]);
  const m0 = git(parent, ["merge-base", "p", "main"]);
  git(parent, ["rebase", "--onto", m1, m0, "p"]);
  const p1b = git(parent, ["rev-parse", "p"]);

  const newTip = new Map([["p", p1b]]);

  // With no way to reach the parent clone, the child can't resolve p1'.
  expect(
    await resolveNewBaseSha(step("p"), "main", newTip, new Map(), child),
  ).toBeNull();

  // Given the parent's worktree path, it fetches p1' in (FETCH_HEAD, no
  // ref created) and the squash-safe replay lands C on it.
  const p1old = git(child, ["rev-parse", "origin/p"]);
  const base = await resolveNewBaseSha(
    step("p"),
    "main",
    newTip,
    new Map([["p", parent]]),
    child,
  );
  expect(base).toBe(p1b);

  const out = await restackEngine.replayStep(
    { branch: "c", worktreePath: child, anchor: p1old, newBase: p1b },
    () => {},
  );
  expect(out.ok).toBe(true);
  if (out.ok) {
    expect(git(child, ["rev-parse", "c~1"])).toBe(p1b); // C sits on p1'
    expect(git(child, ["log", "-1", "--format=%s", "c"])).toBe("C");
  }
});
