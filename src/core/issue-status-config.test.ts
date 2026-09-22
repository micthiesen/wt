import { afterAll, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const root = mkdtempSync(join(tmpdir(), "wt-issue-status-config-"));
afterAll(() => rmSync(root, { recursive: true, force: true }));
const cfg = join(root, "config.toml");
const module = resolve(import.meta.dir, "config.ts");
function load(extra: string) {
  writeFileSync(cfg, `[paths]\nmain_clone=${JSON.stringify(join(root, "main"))}\nworktree_root=${JSON.stringify(join(root, "wts"))}\n[branch]\nprefix="test"\n${extra}`);
  return Bun.spawnSync(["bun", "-e", `import {config} from ${JSON.stringify(module)};console.log(JSON.stringify({tracker:config.issueTracker,actions:config.actions}))`], {
    cwd: root, env: { ...process.env, WT_CONFIG: cfg, WT_REPO_CONFIG: "" }, stdout: "pipe", stderr: "pipe",
  });
}

test("optional status reader and exact shell expectation load without a provider vocabulary", () => {
  const result = load('[issue_tracker]\nstatus_command=["tracker", "statuses", "{ids}", "--json"]\n[[actions]]\nid="move"\nname="Move"\nshell="tracker move {{issue_id}}"\naffects=["issue"]\nissue_status="QA / Customer review"\n');
  expect(result.exitCode, result.stderr.toString()).toBe(0);
  const data = JSON.parse(result.stdout.toString());
  expect(data.tracker.statusCommand).toEqual(["tracker", "statuses", "{ids}", "--json"]);
  expect(data.actions[0].issueStatus).toBe("QA / Customer review");
  for (const extra of ["", "status_command=[]"]) {
    expect(JSON.parse(load(`[issue_tracker]\n${extra}`).stdout.toString()).tracker.statusCommand).toBeNull();
  }
});

test("status reader validates argv and exact standalone expansion", () => {
  for (const value of ['"tracker {ids}"', '["tracker"]', '["{ids}", "read"]', '["tracker", "--ids={ids}"]', '["tracker", "{ids}", "{ids}"]', '["tracker", ""]']) {
    const result = load(`[issue_tracker]\nstatus_command=${value}\n`);
    expect(result.exitCode).not.toBe(0);
    expect(result.stderr.toString()).toContain("issue_tracker.status_command");
  }
});

test("tracker status styles validate exact labels, known icons and RGB colors", () => {
  const result = load('[issue_tracker.status_styles]\n"QA / Customer review"={icon="review",color="#2563EB"}\n');
  expect(result.exitCode, result.stderr.toString()).toBe(0);
  expect(JSON.parse(result.stdout.toString()).tracker.statusStyles).toEqual({ "QA / Customer review": { icon: "review", color: "#2563EB" } });
  for (const value of ['"red"', '{icon="unknown",color="#123456"}', '{icon="circle",color="red"}', '{icon="circle",color="#123456",extra=true}', '{color="#123456"}']) {
    const invalid = load(`[issue_tracker.status_styles]\nOpen=${value}\n`);
    expect(invalid.exitCode).not.toBe(0);
    expect(invalid.stderr.toString()).toContain("issue_tracker.status_styles");
  }
});

test("issue expectation rejects prompt actions, empty status and missing invalidation", () => {
  for (const fields of ['prompt="do it"\naffects=["issue"]\nissue_status="Ready"', 'shell="true"\nissue_status="Ready"', 'shell="true"\naffects=["issue"]\nissue_status=""']) {
    const result = load(`[[actions]]\nid="move"\nname="Move"\n${fields}\n`);
    expect(result.exitCode).not.toBe(0);
    expect(result.stderr.toString()).toContain("issue_status");
  }
});

test("explicit action keys accept digits without changing automatic assignment", () => {
  for (const key of ["0", "1", "9", "a"]) {
    const result = load(`[[actions]]\nid="move"\nname="Move"\nshell="true"\nkey=${JSON.stringify(key)}\n`);
    expect(result.exitCode, result.stderr.toString()).toBe(0);
    expect(JSON.parse(result.stdout.toString()).actions[0].key).toBe(key);
  }
  for (const key of ["A", "!", "10"]) {
    const result = load(`[[actions]]\nid="move"\nname="Move"\nshell="true"\nkey=${JSON.stringify(key)}\n`);
    expect(result.exitCode).not.toBe(0);
    expect(result.stderr.toString()).toContain("actions[0].key");
  }
});
