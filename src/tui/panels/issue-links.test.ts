import { afterAll, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

// Separate processes isolate the config singleton and mocked browser opener.
const root = mkdtempSync(join(tmpdir(), "wt-issue-links-"));
afterAll(() => rmSync(root, { recursive: true, force: true }));
const main = join(root, "main");
mkdirSync(main);
for (const args of [["init", "-q"], ["remote", "add", "origin", "https://github.com/example/tasks.git"]]) {
  const result = Bun.spawnSync(["git", "-C", main, ...args]);
  if (result.exitCode !== 0) throw new Error(result.stderr.toString());
}

const cases = [
  { slug: "coz-12-fix", githubIssue: 456 },
  { slug: "coz-12-fix", githubIssue: 456, issueId: "COZ-99" },
  { slug: "coz-12-fix", githubIssue: 456, issueId: "" },
  { slug: "notes-only", githubIssue: 456 },
  { slug: "eng-3-fix", githubIssue: null },
  { slug: "no-issue", githubIssue: null },
  { slug: "gh-7-fix", githubIssue: null },
];

function links(tracker: boolean) {
  const cfg = join(root, `config-${tracker}.toml`);
  writeFileSync(cfg, `[paths]
main_clone=${JSON.stringify(main)}
worktree_root=${JSON.stringify(join(root, "worktrees"))}
cache_db=${JSON.stringify(join(root, "cache", "cache.sqlite"))}
state_db=${JSON.stringify(join(root, "state.sqlite"))}
[branch]
prefix="test"
${tracker ? '[issue_tracker]\nurl_template="https://tracker.example/{id}"' : ""}
`);
  const module = (path: string) => JSON.stringify(resolve(import.meta.dir, path));
  const script = `
    import { mock } from "bun:test";
    import { Effect } from "effect";
    const macos = await import(${module("../../core/macos.ts")});
    const opened = [];
    let received;
    mock.module(${module("../../core/macos.ts")}, () => ({
      ...macos,
      openUrlHidingTerminal: (url) => Effect.sync(() => { opened.push(url); received(); }),
    }));
    const { preferredIssueUrl } = await import(${module("../../core/issue-tracker.ts")});
    const { yankItemsFor } = await import(${module("yank.tsx")});
    const { handleNormalKey } = await import(${module("../keyboard/normal-keys.ts")});
    const results = [];
    for (const [index, row] of ${JSON.stringify(cases)}.entries()) {
      const url = preferredIssueUrl(row.slug, row.githubIssue, row.issueId);
      const items = yankItemsFor({ ...row, wt: row, fields: { deploy: {}, dev: {} } });
      const receipt = new Promise(resolve => { received = resolve; });
      const count = opened.length;
      handleNormalKey({ name: "i", sequence: "i", raw: "i", ctrl: false, meta: false, shift: false, option: false }, {
        focusedOutputId: null, consumePrTargetChord: () => false, handleGlobalKey: () => false,
        selectedWorktree: { ...row, source: index % 2 ? { kind: "remote", row: { hostLabel: "test" } } : { kind: "local" } },
      });
      if (url) await receipt;
      results.push({ url, opened: opened[count] ?? null, yank: items.find(item => item.key === "i").value,
        primary: items.find(item => item.key === "I").value });
    }
    console.log(JSON.stringify(results));
  `;
  const result = Bun.spawnSync(["bun", "-e", script], {
    cwd: resolve(import.meta.dir, "../../.."),
    env: { ...process.env, WT_CONFIG: cfg, WT_REPO_CONFIG: "", BUN_INSPECT: "" },
    stdout: "pipe", stderr: "pipe", timeout: 10_000,
  });
  expect(result.exitCode, result.stderr.toString()).toBe(0);
  return JSON.parse(result.stdout.toString()) as Array<{
    url: string | null; opened: string | null; yank: string | null; primary: string | null;
  }>;
}

const trackerUrl = (id: string) => `https://tracker.example/${id}`;
const githubUrl = (id: number) => `https://github.com/example/tasks/issues/${id}`;

test("i and y i prefer primary tasks, honor overrides, and fall back to GitHub", () => {
  const expected = [trackerUrl("COZ-12"), trackerUrl("COZ-99"), githubUrl(456), githubUrl(456), trackerUrl("ENG-3"), null, githubUrl(7)];
  const results = links(true);
  expect(results.map(row => row.url)).toEqual(expected);
  expect(results.map(row => row.opened)).toEqual(expected);
  expect(results.map(row => row.yank)).toEqual(expected);
  expect(results.map(row => row.primary)).toEqual([
    trackerUrl("COZ-12"), trackerUrl("COZ-99"), null, null, trackerUrl("ENG-3"), null, githubUrl(7),
  ]);
});

test("missing tracker URL keeps GitHub fallback and bare-ID copying", () => {
  const expected = [githubUrl(456), githubUrl(456), githubUrl(456), githubUrl(456), null, null, githubUrl(7)];
  const results = links(false);
  expect(results.map(row => row.url)).toEqual(expected);
  expect(results.map(row => row.opened)).toEqual(expected);
  expect(results.map(row => row.yank)).toEqual([...expected.slice(0, 4), "ENG-3", null, githubUrl(7)]);
});
