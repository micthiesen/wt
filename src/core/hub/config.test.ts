import { describe, expect, test } from "bun:test";

import { existsSync } from "node:fs";
import { join } from "node:path";

import { buildHubConfig } from "./config.ts";
import { wtArgv } from "./layout.ts";
import { HUB_LEFT_PANE } from "./naming.ts";

describe("hub outer tmux config", () => {
  test("the default tmux prefix is unbound", () => {
    expect(buildHubConfig()).toContain("unbind C-b");
  });

  test("modified keys are forwarded in CSI-u format", () => {
    const config = buildHubConfig();
    expect(config).toContain("set -s extended-keys always");
    expect(config).toContain("set -s extended-keys-format csi-u");
    expect(config).toContain(":extkeys");
  });

  test("a plain letter key forwards via a bare M-<key> bind", () => {
    expect(buildHubConfig()).toContain(`bind -n M-j send-keys -t ${HUB_LEFT_PANE} j`);
  });

  test("a shifted letter key forwards as its own M-<Upper> bind", () => {
    expect(buildHubConfig()).toContain(`bind -n M-G send-keys -t ${HUB_LEFT_PANE} G`);
  });

  test("tmux-special punctuation is quoted safely for config syntax", () => {
    const config = buildHubConfig();
    expect(config).toContain(`bind -n 'M-;' send-keys -t ${HUB_LEFT_PANE} ';'`);
    expect(config).toContain(`bind -n "M-'" send-keys -t ${HUB_LEFT_PANE} "'"`);
  });

  test("F10/F11/F12 are forwarded bare (the left wt process retargets on them)", () => {
    const config = buildHubConfig();
    expect(config).toContain(`bind -n F10 send-keys -t ${HUB_LEFT_PANE} F10`);
    expect(config).toContain(`bind -n F11 send-keys -t ${HUB_LEFT_PANE} F11`);
    expect(config).toContain(`bind -n F12 send-keys -t ${HUB_LEFT_PANE} F12`);
  });

  test("Enter and Tab forward through their named M- bindings", () => {
    const config = buildHubConfig();
    expect(config).toContain(`bind -n M-Enter send-keys -t ${HUB_LEFT_PANE} Enter`);
    expect(config).toContain(`bind -n M-Tab send-keys -t ${HUB_LEFT_PANE} Tab`);
  });

  test("pane resilience: a crashed left or right pane auto-respawns", () => {
    const config = buildHubConfig();
    expect(config).toContain("set -g remain-on-exit on");
    expect(config).toContain("set-hook -g pane-died respawn-pane");
  });
});

describe("wtArgv", () => {
  test("resolves the repo's main entry relative to the module, not argv", () => {
    // In a source checkout (this test environment) the primary branch
    // always wins: [bun, <repo>/src/main.ts], regardless of how the
    // current process was started. The argv-splicing fallback only
    // exists for hypothetical builds without the source tree on disk,
    // which can't be exercised from inside that very source tree.
    const argv = wtArgv();
    expect(argv).toEqual([process.execPath, join(import.meta.dir, "..", "..", "main.ts")]);
    expect(existsSync(argv[1]!)).toBe(true);
  });
});
