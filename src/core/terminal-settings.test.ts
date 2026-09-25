import { describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

function load(settings: string) {
  const root = mkdtempSync("/tmp/wt-terminal-settings-test-");
  try {
    const path = join(root, "user.toml");
    writeFileSync(path, `[paths]
main_clone = ${JSON.stringify(root)}
worktree_root = ${JSON.stringify(join(root, "worktrees"))}
[branch]
prefix = "test"
${settings}`);
    const moduleUrl = (path: string) => JSON.stringify(pathToFileURL(join(import.meta.dir, path)).href);
    const script = `
      const { buildConfig } = await import(${moduleUrl("tmux/config.ts")});
      const { codexHarness } = await import(${moduleUrl("harness/codex/harness.ts")});
      const launches = ["worktree", "main", "manager"].flatMap(slug =>
        [null, "session-id"].map(resumeSessionId => codexHarness.buildArgs({
          slug, resumeSessionId, wtPath: ${JSON.stringify(root)}, managedName: null,
        })));
      console.log(JSON.stringify({ terminal: buildConfig(), launches }));
    `;
    const env: Record<string, string | undefined> = { ...process.env, WT_CONFIG: path };
    delete env.WT_REPO_CONFIG;
    return Bun.spawnSync(["bun", "-e", script], { cwd: root, env, stdout: "pipe", stderr: "pipe" });
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

describe("terminal preferences", () => {
  test("fresh and resumed slots inherit Codex settings without CLI overrides", () => {
    const result = load("");
    expect(result.exitCode).toBe(0);
    const data = JSON.parse(result.stdout.toString());
    expect(data.launches.map((args: string[]) => args[0])).toEqual(Array(6).fill("codex"));
    for (const args of data.launches) expect(args).not.toContain("-c");
    expect(data.launches[0]).toEqual(["codex"]);
    expect(data.launches[1]).toEqual(["codex", "resume", "session-id"]);
    expect(data.terminal).toContain("set -g mouse on");
  });

  test("legacy wt Codex preferences do not override native Codex settings", () => {
    const result = load(`[codex]
animations = true
alternate_screen = "never"
[tmux]
terminal_config = "set -g mouse off"`);
    expect(result.exitCode).toBe(0);
    const data = JSON.parse(result.stdout.toString());
    for (const args of data.launches) {
      expect(args).not.toContain("-c");
      expect(args).not.toContain("--enable");
      expect(args).not.toContain("--disable");
      expect(args).not.toContain("--search");
    }
    expect(data.terminal).toStartWith("set -g mouse off\n");
    expect(data.terminal).not.toContain("set -g mouse on");
    expect(data.terminal).toContain("bind-key -n F12");
  });

  test.each([
    ['[tmux]\nterminal_config = false', "tmux.terminal_config must be a string"],
  ])("rejects invalid settings: %s", (settings, error) => {
    const result = load(settings);
    expect(result.exitCode).not.toBe(0);
    expect(result.stderr.toString()).toContain(error);
  });
});
