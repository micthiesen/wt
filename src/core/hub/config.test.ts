import { describe, expect, test } from "bun:test";

import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { writeIfChanged } from "../tmux/config.ts";
import { buildHubConfig, tmuxQuote } from "./config.ts";
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

  test("a plain letter key forwards via a bare M-<key> bind, wrapped in a command group", () => {
    expect(buildHubConfig()).toContain(`bind -n M-j { send-keys -t ${HUB_LEFT_PANE} j }`);
  });

  test("a shifted letter key forwards as its own M-<Upper> bind, wrapped in a command group", () => {
    expect(buildHubConfig()).toContain(`bind -n M-G { send-keys -t ${HUB_LEFT_PANE} G }`);
  });

  test("tmux-special punctuation is quoted safely for config syntax", () => {
    const config = buildHubConfig();
    expect(config).toContain(`bind -n 'M-;' { send-keys -t ${HUB_LEFT_PANE} ';' }`);
    expect(config).toContain(`bind -n "M-'" { send-keys -t ${HUB_LEFT_PANE} "'" }`);
  });

  test("the M-; binding is wrapped in a { } command group so tmux keeps its trailing ';' argument", () => {
    // Bare `bind -n 'M-;' send-keys -t hub:0.0 ';'` parses fine but tmux
    // drops the trailing `;` argument at config-parse time regardless of
    // quoting, since `;` is tmux's own command separator outside a `{ }`
    // group. Live-verified on a throwaway `-L fixprobe` socket: `list-keys
    // -T root` showed the argument missing under the bare form and present
    // under the `{ }` form.
    const config = buildHubConfig();
    expect(config).toContain(`{ send-keys -t ${HUB_LEFT_PANE} ';' }`);
  });

  test("F10/F11/F12 are forwarded bare (the left wt process retargets on them)", () => {
    const config = buildHubConfig();
    expect(config).toContain(`bind -n F10 { send-keys -t ${HUB_LEFT_PANE} F10 }`);
    expect(config).toContain(`bind -n F11 { send-keys -t ${HUB_LEFT_PANE} F11 }`);
    expect(config).toContain(`bind -n F12 { send-keys -t ${HUB_LEFT_PANE} F12 }`);
  });

  test("Enter and Tab forward through their named M- bindings", () => {
    const config = buildHubConfig();
    expect(config).toContain(`bind -n M-Enter { send-keys -t ${HUB_LEFT_PANE} Enter }`);
    expect(config).toContain(`bind -n M-Tab { send-keys -t ${HUB_LEFT_PANE} Tab }`);
  });

  test("pane resilience: a crashed left or right pane auto-respawns", () => {
    const config = buildHubConfig();
    expect(config).toContain("set -g remain-on-exit on");
    expect(config).toContain("set-hook -g pane-died respawn-pane");
  });
});

describe("tmuxQuote", () => {
  test("a token with no special characters is left bare", () => {
    expect(tmuxQuote("j")).toBe("j");
  });

  test("a token containing only a single quote switches to double-quote delimiters", () => {
    expect(tmuxQuote("it's")).toBe(`"it's"`);
  });

  test("a token containing only a double quote switches to single-quote delimiters", () => {
    expect(tmuxQuote('say "hi"')).toBe(`'say "hi"'`);
  });

  test("a token containing both quote characters escapes embedded double quotes", () => {
    // Neither bare delimiter is safe once a token has both `'` and `"` in
    // it — double-quoting without escaping (the pre-fix behavior) would
    // terminate the tmux string early at the token's own embedded `"`,
    // truncating the config-file argument.
    const token = `it's "quoted"`;
    expect(tmuxQuote(token)).toBe(`"it's \\"quoted\\""`);
  });
});

describe("writeIfChanged", () => {
  test("writes on first call and reports changed", () => {
    const dir = mkdtempSync(join(tmpdir(), "wt-hub-config-test-"));
    try {
      const path = join(dir, "test.conf");
      const result = writeIfChanged(path, "hello\n");
      expect(result).toEqual({ path, changed: true });
      expect(readFileSync(path, "utf8")).toBe("hello\n");
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  test("reports unchanged and doesn't rewrite when content is identical", () => {
    const dir = mkdtempSync(join(tmpdir(), "wt-hub-config-test-"));
    try {
      const path = join(dir, "test.conf");
      writeIfChanged(path, "hello\n");
      const result = writeIfChanged(path, "hello\n");
      expect(result).toEqual({ path, changed: false });
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
  });

  test("reports changed and rewrites when content differs", () => {
    const dir = mkdtempSync(join(tmpdir(), "wt-hub-config-test-"));
    try {
      const path = join(dir, "test.conf");
      writeIfChanged(path, "hello\n");
      const result = writeIfChanged(path, "goodbye\n");
      expect(result).toEqual({ path, changed: true });
      expect(readFileSync(path, "utf8")).toBe("goodbye\n");
    } finally {
      rmSync(dir, { recursive: true, force: true });
    }
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
