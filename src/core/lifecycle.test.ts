import { expect, test } from "bun:test";

import { chmodSync, existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

import { REPOSITORY_CONFIG_ENV } from "./config-layer.ts";
import { git, trackedTmpDirs } from "./test-fixtures.ts";

const { tmp } = trackedTmpDirs();

test("remove teardown keeps checkout-dependent cleanup before removal and browser cleanup after", () => {
  const source = readFileSync(join(import.meta.dir, "lifecycle.ts"), "utf8");
  const teardown = source.indexOf(
    "const destroyCommand = resolveTeardownCommand",
  );
  const reaper = source.indexOf("const reaped = yield* reapWorktreeListeners");
  const backend = source.indexOf("const removed = yield* backend.remove");
  const browser = source.indexOf(
    "const browser = yield* closeWorktreeBrowserSessions",
    backend,
  );
  expect(teardown).toBeGreaterThan(0);
  expect(reaper).toBeGreaterThan(teardown);
  expect(backend).toBeGreaterThan(reaper);
  expect(browser).toBeGreaterThan(backend);
}, 20_000);

test("createWorktree copies configured glob matches from the main clone", () => {
  const root = tmp("wt-copy-globs-");
  const origin = join(root, "origin.git");
  const main = join(root, "main");
  const worktrees = join(root, "worktrees");
  const configPath = join(root, "config.toml");

  mkdirSync(origin);
  git(origin, ["init", "-q", "--bare"]);
  git(root, ["clone", "-q", origin, main]);
  git(main, ["checkout", "-q", "-b", "main"]);
  writeFileSync(join(main, ".gitignore"), ".agents/\n.cache/\n");
  writeFileSync(join(main, "README.md"), "fixture\n");
  git(main, ["add", ".gitignore", "README.md"]);
  git(main, ["commit", "-q", "-m", "fixture"]);
  git(main, ["push", "-q", "-u", "origin", "main"]);
  git(main, ["branch", "test/existing-branch"]);

  const skill = join(main, ".agents", "skills", "example", "SKILL.md");
  mkdirSync(join(main, ".agents", "skills", "example"), { recursive: true });
  writeFileSync(skill, "agent skill\n");
  mkdirSync(join(main, ".cache"), { recursive: true });
  writeFileSync(join(main, ".cache", "private.txt"), "do not copy\n");

  writeFileSync(
    configPath,
    `
[paths]
main_clone = ${JSON.stringify(main)}
worktree_root = ${JSON.stringify(worktrees)}
log_dir = ${JSON.stringify(join(root, "logs"))}
lock_dir = ${JSON.stringify(join(root, "locks"))}
cache_db = ${JSON.stringify(join(root, "cache.sqlite"))}
state_db = ${JSON.stringify(join(root, "state.sqlite"))}

[branch]
prefix = "test"
base = "main"

[lifecycle]
env_files_to_copy = []
copy_globs = [".agents/**", ".git/**", "./.git/**"]
install_command = ${JSON.stringify(`touch ${join(root, "install-started")}; printf '%s\\n' "$WT_INSTALL_READY"; exec sleep 3`)}
`,
  );

  const fakeBin = join(root, "bin");
  mkdirSync(fakeBin);
  const gitWrapper = join(fakeBin, "git");
  const realGit = Bun.which("git")!;
  writeFileSync(gitWrapper, `#!/bin/sh
case "$*" in
  *backend-interrupted*) touch ${JSON.stringify(join(root, "backend-create-started"))}; exec sleep 3 ;;
  "worktree remove "*) touch ${JSON.stringify(join(root, "backend-remove-started"))}; exec sleep 3 ;;
esac
exec ${JSON.stringify(realGit)} "$@"
`);
  chmodSync(gitWrapper, 0o755);
  const pnpmWrapper = join(fakeBin, "pnpm");
  writeFileSync(pnpmWrapper, `#!/bin/sh
touch ${JSON.stringify(join(root, "sst-remove-started"))}
exec sleep 3
`);
  chmodSync(pnpmWrapper, 0o755);

  const lifecycleModule = pathToFileURL(
    join(import.meta.dir, "lifecycle.ts"),
  ).href;
  const effectModule = pathToFileURL(
    join(import.meta.dir, "../../node_modules/effect/dist/index.js"),
  ).href;
  const script = `
    const { Effect } = await import(${JSON.stringify(effectModule)});
    const { createWorktree } = await import(${JSON.stringify(lifecycleModule)});
    const { readWtState } = await import(${JSON.stringify(pathToFileURL(join(import.meta.dir, "wtstate.ts")).href)});
    const createOrFail = (branch, opts) => Effect.runPromise(createWorktree(branch, opts).pipe(
      Effect.catchTag("LifecycleError", (e) => Effect.succeed({ ok: false, reason: e.message })),
    ));
    const { lockStatus } = await import(${JSON.stringify(pathToFileURL(join(import.meta.dir, "locks.ts")).href)});
    const result = await createOrFail("test/copy-agents", { runInstall: false });
    const attached = await createOrFail("test/existing-branch", { runInstall: false });
    const failed = await createOrFail("test/bad-base", {
      runInstall: false,
      base: "missing-ref-that-does-not-exist",
    });
    const controller = new AbortController();
    const interrupted = await Effect.runPromiseExit(
      createWorktree("test/interrupted", {
        runInstall: false,
        onPhase: () => controller.abort(),
      }),
      { signal: controller.signal },
    );
    const backendController = new AbortController();
    const backendInterrupted = await Effect.runPromiseExit(
      createWorktree("test/backend-interrupted", {
        runInstall: false,
        onLog: (line) => {
          if (line.includes("new branch test/backend-interrupted")) {
            setTimeout(() => backendController.abort(), 40);
          }
        },
      }),
      { signal: backendController.signal },
    );
    const installController = new AbortController();
    const installInterrupted = await Effect.runPromiseExit(
      createWorktree("test/install-interrupted", {
        onLog: (line) => {
          if (line === "install-ready") setTimeout(() => installController.abort(), 40);
        },
      }),
      { signal: installController.signal },
    );
    const removeController = new AbortController();
    const creationState = readWtState().slugs;
    const removeInterrupted = await Effect.runPromiseExit(
      (await import(${JSON.stringify(lifecycleModule)})).removeWorktree(
        { ...result, isMain: false },
        { onPhase: (phase) => {
          if (phase.startsWith("worktree remove")) setTimeout(() => removeController.abort(), 40);
        } },
      ),
      { signal: removeController.signal },
    );
    const sstCreated = await createOrFail("test/sst-interrupted", { runInstall: false });
    (await import("node:fs")).mkdirSync(${JSON.stringify(join(worktrees, "sst-interrupted", ".sst"))}, { recursive: true });
    await Bun.write(${JSON.stringify(join(worktrees, "sst-interrupted", ".sst", "stage"))}, ${JSON.stringify("test-owned\n")});
    const sstController = new AbortController();
    const sstInterrupted = await Effect.runPromiseExit(
      (await import(${JSON.stringify(lifecycleModule)})).removeWorktree(
        { ...sstCreated, isMain: false },
        { destroyStage: true, onPhase: (phase) => {
          if (phase === "sst remove") setTimeout(() => sstController.abort(), 40);
        } },
      ),
      { signal: sstController.signal },
    );
    console.log(JSON.stringify({
      result,
      created: typeof creationState["copy-agents"]?.createdAt === "string",
      attachedCreated: attached.ok && typeof creationState["existing-branch"]?.createdAt === "string",
      failedCreated: Boolean(creationState["bad-base"]?.createdAt || creationState["install-interrupted"]?.createdAt || creationState["backend-interrupted"]?.createdAt),
      failed,
      interrupted: interrupted._tag,
      backendInterrupted: backendInterrupted._tag,
      installInterrupted: installInterrupted._tag,
      removeInterrupted: removeInterrupted._tag,
      sstInterrupted: sstInterrupted._tag,
      locks: {
        success: lockStatus("copy-agents"),
        failure: lockStatus("bad-base"),
        interruption: lockStatus("interrupted"),
        backend: lockStatus("backend-interrupted"),
        install: lockStatus("install-interrupted"),
        remove: lockStatus("copy-agents"),
        sst: lockStatus("sst-interrupted"),
      },
    }));
  `;
  const env: Record<string, string | undefined> = {
    ...process.env,
    WT_CONFIG: configPath,
    PATH: `${fakeBin}:${process.env.PATH ?? ""}`,
    GIT_CONFIG_GLOBAL: "/dev/null",
    GIT_CONFIG_SYSTEM: "/dev/null",
    GIT_AUTHOR_NAME: "wt test",
    GIT_AUTHOR_EMAIL: "wt@example.test",
    GIT_COMMITTER_NAME: "wt test",
    GIT_COMMITTER_EMAIL: "wt@example.test",
    WT_INSTALL_READY: "install-ready",
  };
  delete env[REPOSITORY_CONFIG_ENV];
  const result = Bun.spawnSync([process.execPath, "-e", script], {
    cwd: root,
    env,
    stdout: "pipe",
    stderr: "pipe",
  });

  expect(result.exitCode, result.stderr.toString()).toBe(0);
  expect(JSON.parse(result.stdout.toString())).toMatchObject({
    result: { ok: true },
    created: true,
    attachedCreated: true,
    failedCreated: false,
    failed: { ok: false },
    interrupted: "Failure",
    backendInterrupted: "Failure",
    installInterrupted: "Failure",
    removeInterrupted: "Failure",
    sstInterrupted: "Failure",
    locks: { success: null, failure: null, interruption: null, backend: null, install: null, remove: null, sst: null },
  });
  expect(existsSync(join(root, "backend-create-started"))).toBe(true);
  expect(existsSync(join(worktrees, "backend-interrupted"))).toBe(false);
  expect(existsSync(join(root, "install-started"))).toBe(true);
  expect(existsSync(join(root, "install-finished"))).toBe(false);
  expect(existsSync(join(root, "backend-remove-started"))).toBe(true);
  expect(existsSync(join(worktrees, "copy-agents"))).toBe(true);
  expect(existsSync(join(root, "sst-remove-started"))).toBe(true);
  expect(existsSync(join(worktrees, "sst-interrupted"))).toBe(true);
  const copied = join(
    worktrees,
    "copy-agents",
    ".agents",
    "skills",
    "example",
    "SKILL.md",
  );
  expect(readFileSync(copied, "utf8")).toBe("agent skill\n");
  expect(
    existsSync(join(worktrees, "copy-agents", ".cache", "private.txt")),
  ).toBe(false);
}, 20_000);
