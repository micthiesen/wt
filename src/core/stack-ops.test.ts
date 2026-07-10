/**
 * resolveAnchor: the squash-safe replay cut point survives a
 * hand-rebase. Two stale-anchor shapes a bare `--is-ancestor` guard
 * couldn't tell apart, pinned against real git repos.
 */
import { afterAll, expect, test } from "bun:test";

import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { resolveAnchor } from "./stack-ops.ts";

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
  return new TextDecoder().decode(r.stdout);
}

test("resolveAnchor uses the live merge-base when a branch was rebased onto newer trunk", async () => {
  // eng-5244 reproduction: the branch was hand-rebased onto a newer parent
  // tip, so the OLD baseSha (p1) is STILL an ancestor of the branch — the
  // naive guard trusts it and replays already-present history. The real
  // fork point advanced to p2.
  const dir = mkdtempSync(join(tmpdir(), "wt-anchor-test-"));
  dirs.push(dir);
  git(dir, ["init", "-q", "-b", "main"]);
  git(dir, ["commit", "-q", "--allow-empty", "-m", "M0"]);
  git(dir, ["checkout", "-q", "-b", "p"]);
  git(dir, ["commit", "-q", "--allow-empty", "-m", "P1"]);
  const p1 = git(dir, ["rev-parse", "HEAD"]).trim();
  git(dir, ["commit", "-q", "--allow-empty", "-m", "P2"]);
  const p2 = git(dir, ["rev-parse", "HEAD"]).trim();
  git(dir, ["checkout", "-q", "-b", "c"]); // c built on the advanced parent tip
  git(dir, ["commit", "-q", "--allow-empty", "-m", "C"]);

  const anchor = await resolveAnchor({ branch: "c", baseSha: p1 }, "p", dir);
  expect(anchor).toBe(p2); // the true fork point, so only C replays
  expect(anchor).not.toBe(p1); // not the stale stored anchor
});

test("resolveAnchor keeps baseSha when the live merge-base is older (squash-merged parent)", async () => {
  // The healthy squash case: the parent squash-merged into the integration
  // branch as one commit, so merge-base(child, parent) drops to M0 — BELOW
  // the recorded baseSha. baseSha must stand, or the squashed parent's
  // commits get re-applied.
  const dir = mkdtempSync(join(tmpdir(), "wt-anchor-test-"));
  dirs.push(dir);
  git(dir, ["init", "-q", "-b", "main"]);
  git(dir, ["commit", "-q", "--allow-empty", "-m", "M0"]);
  const m0 = git(dir, ["rev-parse", "HEAD"]).trim();
  git(dir, ["checkout", "-q", "-b", "p"]);
  git(dir, ["commit", "-q", "--allow-empty", "-m", "P1"]);
  const p1 = git(dir, ["rev-parse", "HEAD"]).trim();
  git(dir, ["checkout", "-q", "-b", "c"]); // child built on the parent tip
  git(dir, ["commit", "-q", "--allow-empty", "-m", "C"]);
  git(dir, ["checkout", "-q", "-b", "released", "main"]);
  git(dir, ["commit", "-q", "--allow-empty", "-m", "squash of p"]);

  const anchor = await resolveAnchor({ branch: "c", baseSha: p1 }, "released", dir);
  expect(anchor).toBe(p1); // squash-safe anchor preserved
  expect(anchor).not.toBe(m0);
});
