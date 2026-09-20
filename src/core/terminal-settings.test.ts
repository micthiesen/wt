import { describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { pathToFileURL } from "node:url";

function load(settings: string, repositorySettings?: string) {
  const root = mkdtempSync("/tmp/wt-terminal-settings-test-");
  try {
    const path = join(root, "user.toml");
    writeFileSync(path, `[paths]
main_clone = ${JSON.stringify(root)}
worktree_root = ${JSON.stringify(join(root, "worktrees"))}
[branch]
prefix = "test"
${settings}`);
    if (repositorySettings !== undefined) writeFileSync(join(root, ".wt.toml"), repositorySettings);
    const moduleUrl = (path: string) => JSON.stringify(pathToFileURL(join(import.meta.dir, path)).href);
    const script = `
      const { config } = await import(${moduleUrl("config.ts")});
      const { buildConfig } = await import(${moduleUrl("tmux/config.ts")});
      const { codexHarness } = await import(${moduleUrl("harness/codex/harness.ts")});
      const launches = ["worktree", "main", "manager"].flatMap(slug =>
        [null, "session-id"].map(resumeSessionId => codexHarness.buildArgs({
          slug, resumeSessionId, wtPath: ${JSON.stringify(root)}, managedName: null,
        })));
      console.log(JSON.stringify({ codex: config.codex, terminal: buildConfig(), launches }));
    `;
    const env: Record<string, string | undefined> = { ...process.env, WT_CONFIG: path };
    delete env.WT_REPO_CONFIG;
    return Bun.spawnSync(["bun", "-e", script], { cwd: root, env, stdout: "pipe", stderr: "pipe" });
  } finally {
    rmSync(root, { recursive: true, force: true });
  }
}

describe("terminal preferences", () => {
  test("upstream defaults suppress animations for all fresh and resumed slots", () => {
    const result = load("");
    expect(result.exitCode).toBe(0);
    const data = JSON.parse(result.stdout.toString());
    expect(data.codex).toEqual({ animations: false, alternateScreen: "always" });
    for (const args of data.launches) {
      expect(args).toContain("tui.animations=false");
      expect(args).toContain('tui.alternate_screen="always"');
    }
    expect(data.terminal).toContain("set -g mouse on");
  });

  test("personal preferences reach the real launch and tmux render paths", () => {
    const result = load(`[codex]
animations = true
alternate_screen = "never"
[tmux]
terminal_config = "set -g mouse off"`);
    expect(result.exitCode).toBe(0);
    const data = JSON.parse(result.stdout.toString());
    for (const args of data.launches) {
      expect(args).toContain("tui.animations=true");
      expect(args).toContain('tui.alternate_screen="never"');
      expect(args).not.toContain("tui.animations=false");
    }
    expect(data.terminal).toStartWith("set -g mouse off\n");
    expect(data.terminal).not.toContain("set -g mouse on");
    expect(data.terminal).toContain("bind-key -n F12");
  });

  test("repository overrides retain unspecified personal preferences", () => {
    const result = load('[codex]\nanimations = true\nalternate_screen = "auto"', '[codex]\nanimations = false');
    expect(result.exitCode).toBe(0);
    expect(JSON.parse(result.stdout.toString()).codex).toEqual({ animations: false, alternateScreen: "auto" });
  });

  test.each([
    ['[codex]\nanimations = "true"', "codex.animations must be a boolean"],
    ['[codex]\nalternate_screen = "sometimes"', "codex.alternate_screen must be one of"],
    ['[tmux]\nterminal_config = false', "tmux.terminal_config must be a string"],
  ])("rejects invalid settings: %s", (settings, error) => {
    const result = load(settings);
    expect(result.exitCode).not.toBe(0);
    expect(result.stderr.toString()).toContain(error);
  });
});
