import { afterAll, expect, test } from "bun:test";
import { chmodSync, existsSync, mkdirSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

const roots: string[] = [];
afterAll(() => {
  for (const root of roots) rmSync(root, { recursive: true, force: true });
});

const WTSTATE = JSON.stringify(pathToFileURL(join(import.meta.dir, "wtstate.ts")).href);
const STATE_DB = JSON.stringify(pathToFileURL(join(import.meta.dir, "state-db.ts")).href);

function run(cwd: string, userConfig: string, script: string) {
  const env: Record<string, string | undefined> = { ...process.env, WT_CONFIG: userConfig };
  delete env.WT_REPO_CONFIG;
  return Bun.spawnSync([process.execPath, "-e", script], {
    cwd,
    env,
    stdout: "pipe",
    stderr: "pipe",
  });
}

function writeRepoConfig(path: string, main: string, stateDb: string): void {
  mkdirSync(path, { recursive: true });
  writeFileSync(join(path, ".wt.toml"), `
[paths]
main_clone = ${JSON.stringify(main)}
worktree_root = ${JSON.stringify(`${main}-worktrees`)}
cache_db = ${JSON.stringify(join(path, ".cache", "cache.sqlite"))}
state_db = ${JSON.stringify(stateDb)}

[tmux]
socket = ${JSON.stringify(`wt-test-${path.replace(/[^a-z0-9]/gi, "-")}`)}
`);
}

test("one database isolates repository state by path-derived id", () => {
  const root = mkdtempSync(join(tmpdir(), "wt-state-db-"));
  roots.push(root);
  const db = join(root, "state", "wt.sqlite");
  const user = join(root, "config.toml");
  writeFileSync(user, "[branch]\nprefix = \"test\"\n");
  const one = join(root, "one");
  const two = join(root, "two");
  writeRepoConfig(one, one, db);
  writeRepoConfig(two, two, db);

  const firstWrite = run(one, user, `
    const state = await import(${WTSTATE});
    state.setSlugSection("shared-slug", "one-section");
  `);
  expect(firstWrite.exitCode, firstWrite.stderr.toString()).toBe(0);
  const secondWrite = run(two, user, `
    const state = await import(${WTSTATE});
    state.setSlugSection("shared-slug", "two-section");
  `);
  expect(secondWrite.exitCode, secondWrite.stderr.toString()).toBe(0);

  const firstRead = run(one, user, `
    const state = await import(${WTSTATE});
    console.log(state.readWtState().slugs["shared-slug"].section);
  `);
  expect(firstRead.exitCode, firstRead.stderr.toString()).toBe(0);
  expect(firstRead.stdout.toString().trim()).toBe("one-section");
});

test("reads existing state without write access or repository timestamp updates", () => {
  const root = mkdtempSync(join(tmpdir(), "wt-state-readonly-"));
  roots.push(root);
  const db = join(root, "state", "wt.sqlite");
  const user = join(root, "config.toml");
  const repo = join(root, "repo");
  writeFileSync(user, "[branch]\nprefix = \"test\"\n");
  writeRepoConfig(repo, repo, db);

  const seeded = run(repo, user, `
    const state = await import(${WTSTATE});
    state.readWtState();
    state.setSlugSection("existing", "Test Section");
  `);
  expect(seeded.exitCode, seeded.stderr.toString()).toBe(0);
  const before = run(repo, user, `
    const { Database } = await import("bun:sqlite");
    const db = new Database(${JSON.stringify(db)}, { readonly: true });
    console.log(db.query("SELECT updated_at FROM repositories LIMIT 1").get().updated_at);
  `);
  expect(before.exitCode, before.stderr.toString()).toBe(0);

  chmodSync(db, 0o444);
  chmodSync(join(root, "state"), 0o555);
  try {
    const read = run(repo, user, `
      const state = await import(${WTSTATE});
      const database = await import(${STATE_DB});
      console.log(state.readWtState().slugs.existing.section);
      console.log(String(database.readArchivedKeys().size));
    `);
    expect(read.exitCode, read.stderr.toString()).toBe(0);
    expect(read.stdout.toString().trim()).toBe("Test Section\n0");
    const after = run(repo, user, `
      const { Database } = await import("bun:sqlite");
      const db = new Database(${JSON.stringify(db)}, { readonly: true });
      console.log(db.query("SELECT updated_at FROM repositories LIMIT 1").get().updated_at);
    `);
    expect(after.stdout.toString()).toBe(before.stdout.toString());
  } finally {
    chmodSync(join(root, "state"), 0o755);
    chmodSync(db, 0o644);
  }
});

test("reading a repository with no state database does not create one", () => {
  const root = mkdtempSync(join(tmpdir(), "wt-state-absent-"));
  roots.push(root);
  const db = join(root, "state", "wt.sqlite");
  const user = join(root, "config.toml");
  const repo = join(root, "repo");
  writeFileSync(user, "[branch]\nprefix = \"test\"\n");
  writeRepoConfig(repo, repo, db);
  const read = run(repo, user, `
    const state = await import(${WTSTATE});
    console.log(state.readWtState().version);
  `);
  expect(read.exitCode, read.stderr.toString()).toBe(0);
  expect(existsSync(db)).toBe(false);
});

test("controller keeps host-qualified remote layout separate from local slug state", () => {
  const root = mkdtempSync(join(tmpdir(), "wt-remote-layout-"));
  roots.push(root);
  const db = join(root, "state", "wt.sqlite");
  const user = join(root, "config.toml");
  const repo = join(root, "repo");
  writeFileSync(user, "[branch]\nprefix = \"test\"\n");
  writeRepoConfig(repo, repo, db);

  const write = run(repo, user, `
    const state = await import(${WTSTATE});
    state.setSlugSection("same-slug", "Local");
    state.setWorktreeSection("@remote/dellserver/same-slug", "Remote");
    console.log(JSON.stringify(state.readWtState()));
  `);
  expect(write.exitCode, write.stderr.toString()).toBe(0);
  const state = JSON.parse(write.stdout.toString());
  expect(state.slugs["same-slug"].section).toBe("Local");
  expect(state.remoteLayouts["@remote/dellserver/same-slug"].section).toBe("Remote");

  const reap = run(repo, user, `
    const state = await import(${WTSTATE});
    state.reapRemoteLayouts("dellserver", new Set());
    console.log(JSON.stringify(state.readWtState()));
  `);
  expect(reap.exitCode, reap.stderr.toString()).toBe(0);
  const reaped = JSON.parse(reap.stdout.toString());
  expect(reaped.slugs["same-slug"].section).toBe("Local");
  expect(reaped.remoteLayouts).toEqual({});
});

test("a namespace collision refuses the second canonical repository path", () => {
  const root = mkdtempSync(join(tmpdir(), "wt-state-collision-"));
  roots.push(root);
  const db = join(root, "state.sqlite");
  const user = join(root, "config.toml");
  writeFileSync(user, "[branch]\nprefix = \"test\"\n");
  const flat = join(root, "a-b");
  const nested = join(root, "a", "b");
  writeRepoConfig(flat, flat, db);
  writeRepoConfig(nested, nested, db);

  const first = run(flat, user, `
    const state = await import(${WTSTATE});
    state.setSlugSection("owned", "flat");
  `);
  expect(first.exitCode, first.stderr.toString()).toBe(0);
  const second = run(nested, user, `
    const state = await import(${WTSTATE});
    state.readWtState();
  `);
  expect(second.exitCode).not.toBe(0);
  expect(second.stderr.toString()).toContain("repository namespace collision");
});
