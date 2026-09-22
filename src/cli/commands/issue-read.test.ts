import { afterAll, expect, test } from "bun:test";
import { mkdirSync, mkdtempSync, realpathSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

const root = realpathSync(mkdtempSync(join(tmpdir(), "wt-task-read-")));
afterAll(() => rmSync(root, { recursive: true, force: true }));
const main = join(root, "main");
const target = join(root, "wts", "eng-12-example");
const cfg = join(root, "config.toml");
const reader = join(root, "reader.ts");
const launcher = resolve(import.meta.dir, "../../..", "bin/wt");
const env = { ...process.env, WT_CONFIG: cfg, WT_REPO_CONFIG: "" };
function exec(argv: string[], cwd = root) {
  return Bun.spawnSync(argv, { cwd, env, stdout: "pipe", stderr: "pipe" });
}
function cli(args: string[], cwd = target) {
  return exec(["sh", launcher, "issue", ...args], cwd);
}
function config(readerToml = JSON.stringify(["bun", reader, "{id}"])) {
  writeFileSync(cfg, `[paths]\nmain_clone=${JSON.stringify(main)}\nworktree_root=${JSON.stringify(join(root, "wts"))}\ncache_db=${JSON.stringify(join(root, "cache.sqlite"))}\nstate_db=${JSON.stringify(join(root, "state.sqlite"))}\n[branch]\nprefix="t"\n[issue_tracker]\nread_command=${readerToml}\n`);
}
mkdirSync(main);
expect(exec(["git", "init", "-b", "main", main]).exitCode).toBe(0);
expect(exec(["git", "-c", "core.hooksPath=/dev/null", "-c", "user.name=Test", "-c", "user.email=test@example.invalid", "commit", "--allow-empty", "-m", "fixture"], main).exitCode).toBe(0);
expect(exec(["git", "worktree", "add", "-b", "t/eng-12-example", target], main).exitCode).toBe(0);
writeFileSync(reader, 'console.log(JSON.stringify({id:process.argv[2],cwd:process.cwd()}));');

test("installed launcher reads the resolved task in its cwd, including override and explicit none", () => {
  config();
  let read = cli(["--read"]);
  expect(read.exitCode, read.stderr.toString()).toBe(0);
  expect(JSON.parse(read.stdout.toString())).toEqual({ id: "ENG-12", cwd: target });
  const nested = join(target, "nested");
  mkdirSync(nested);
  expect(JSON.parse(cli(["--read"], nested).stdout.toString()).cwd).toBe(target);
  expect(cli(["eng-12-example", "--id", "ENG-99"]).exitCode).toBe(0);
  read = cli(["t/eng-12-example", "--read"], main);
  expect(read.exitCode, read.stderr.toString()).toBe(0);
  expect(JSON.parse(read.stdout.toString()).id).toBe("ENG-99");
  expect(cli(["eng-12-example", "--no-id"]).exitCode).toBe(0);
  read = cli(["--read"]);
  expect(read.exitCode).toBe(0);
  expect(read.stdout.toString()).toBe("");
  expect(read.stderr.toString()).toContain("no tracker task attached");
  expect(cli(["eng-12-example", "--clear-id"]).exitCode).toBe(0);
});

test("unconfigured reader is distinct and normal identity lookup remains offline", () => {
  config("[]");
  expect(cli(["--read"]).exitCode).toBe(3);
  expect(cli(["eng-12-example"]).exitCode).toBe(0);
});

test("reader failure preserves partial output but never reports full context", () => {
  config(JSON.stringify(["bun", "-e", 'console.log("partial");console.error("file denied");process.exit(7)', "{id}"]));
  const read = cli(["--read"]);
  expect(read.exitCode).toBe(7);
  expect(read.stdout.toString()).toBe("partial\n");
  expect(read.stderr.toString()).toContain("file denied");
  expect(read.stderr.toString()).toContain("task read incomplete for ENG-12");
  config(JSON.stringify(["bun", "-e", "process.exit(3)", "{id}"]));
  const reserved = cli(["--read"]);
  expect(reserved.exitCode).toBe(1);
  expect(reserved.stderr.toString()).toContain("reader exited 3");
});

test("invalid reader configuration fails before invoking anything", () => {
  for (const value of ['"tracker {id}"', '["tracker"]', '["{id}", "read"]', '["tracker", ""]']) {
    config(value);
    const read = cli(["--read"]);
    expect(read.exitCode).not.toBe(0);
    expect(read.stderr.toString()).toContain("issue_tracker.read_command");
  }
});
