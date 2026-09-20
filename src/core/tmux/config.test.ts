import { describe, expect, test } from "bun:test";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { buildConfig, TERMINAL_PREAMBLE } from "./config.ts";
import { codexPaneOptionArgs } from "./attach.ts";
import { sessionSwitchTarget } from "./naming.ts";

describe("worktree session shortcut routing", () => {
  test("a pinned preamble reproduces defaults and a replacement retains palette and navigation", () => {
    expect(buildConfig("", TERMINAL_PREAMBLE)).toBe(buildConfig("", null));
    const custom = buildConfig("set -g window-style bg=#123456", "set -g mouse off");
    expect(custom).toStartWith("set -g mouse off\n");
    expect(custom).not.toContain("set -g mouse on");
    expect(custom).toContain("set -g window-style bg=#123456");
    expect(custom).toContain("bind-key -n F12");
    expect(buildConfig("", "")).not.toContain("set -g alternate-screen");
  });

  test("allows full-screen harness TUIs to use the alternate screen", () => {
    const config = buildConfig();
    expect(config).toContain("set -g alternate-screen on");
    expect(config).not.toContain("set -g alternate-screen off");
  });

  test("the owning F-key detaches and cross-session keys request a switch", () => {
    const config = buildConfig();
    expect(config).toContain(
      "F10 if-shell -F '#{==:#{@wt-shortcut},shell}' 'detach-client'",
    );
    expect(config).toContain(
      "F11 if-shell -F '#{==:#{@wt-shortcut},diff}' 'detach-client'",
    );
    expect(config).toContain(
      "F12 if-shell -F '#{==:#{@wt-shortcut},harness}' 'detach-client'",
    );
  });

  test("modified keys are forwarded in CSI-u format", () => {
    const config = buildConfig();
    expect(config).toContain("set -s extended-keys always");
    expect(config).toContain("set -sq extended-keys-format csi-u");
    expect(config).toContain(":extkeys");
  });

  test("OSC 8 hyperlink boundaries are forwarded to the outer terminal", () => {
    const config = buildConfig();
    expect(config).toContain(
      "xterm*:hyperlinks,tmux-256color:hyperlinks",
    );
    expect(config).toContain(
      "MouseDown1Pane if-shell -F '#{!=:#{mouse_hyperlink},}'",
    );
    expect(config).toContain(
      `'run-shell -b "/usr/bin/open #{q:mouse_hyperlink}"'`,
    );
    expect(config).toContain("'select-pane -t = \\; send-keys -M'");
  });

  test("mouse selections copy to the macOS clipboard on release", () => {
    const config = buildConfig();
    expect(config).toContain(
      "bind-key -T copy-mode MouseDragEnd1Pane send-keys -X copy-pipe-and-cancel pbcopy",
    );
    expect(config).toContain(
      "bind-key -T copy-mode-vi MouseDragEnd1Pane send-keys -X copy-pipe-and-cancel pbcopy",
    );
  });

  test("private tmux-client exit statuses decode to their targets", () => {
    expect(sessionSwitchTarget(110)).toBe("shell");
    expect(sessionSwitchTarget(111)).toBe("diff");
    expect(sessionSwitchTarget(112)).toBe("harness");
    expect(sessionSwitchTarget(0)).toBeNull();
    expect(sessionSwitchTarget(null)).toBeNull();
  });
});

test.skipIf(!Bun.which("tmux"))("real xterm client receives synchronized redraws and native cursor defaults", async () => {
  const dir = mkdtempSync(join(tmpdir(), "wt-sync-redraw-"));
  const socket = join(dir, "socket");
  const configPath = join(dir, "tmux.conf");
  writeFileSync(configPath, buildConfig());
  let output = "";
  const { promise: ready, resolve: markReady } = Promise.withResolvers<void>();
  const client = Bun.spawn([
    "tmux", "-S", socket, "-f", configPath, "new-session", "-s", "probe",
    "bash", "-c", "printf WT_SYNC_READY; read -r ignored",
  ], {
    env: { ...process.env, TERM: "xterm-256color", TMUX: "" },
    terminal: {
      cols: 80, rows: 24,
      data(_terminal, data) {
        output += Buffer.from(data).toString();
        if (output.includes("WT_SYNC_READY") && output.includes("\x1b[?2026h") && output.includes("\x1b[?2026l")) markReady();
      },
    },
  });
  const timer = setTimeout(markReady, 3000);
  const command = async (...args: string[]) => {
    const proc = Bun.spawn(["tmux", "-S", socket, ...args], { stdout: "pipe", stderr: "pipe" });
    const [stdout, stderr, code] = await Promise.all([
      new Response(proc.stdout).text(), new Response(proc.stderr).text(), proc.exited,
    ]);
    expect(stderr).toBe("");
    expect(code).toBe(0);
    return stdout;
  };
  try {
    await ready;
    expect(output).toContain("WT_SYNC_READY");
    expect(await command("list-clients", "-F", "#{client_termfeatures}")).toMatch(/\bsync\b/);
    expect(output).toContain("\x1b[?2026h");
    expect(output).toContain("\x1b[?2026l");
    await command("set-option", "-p", "-t", "probe", "cursor-style", "block");
    await command(...codexPaneOptionArgs("codex", "probe"));
    expect((await command("show-options", "-p", "-v", "-t", "probe", "cursor-style")).trim()).toBe("");
  } finally {
    clearTimeout(timer);
    try {
      // Startup failure can mean there is no server to kill. Cleanup must
      // not mask the assertion or skip closing the PTY in that case.
      await Bun.spawn(["tmux", "-S", socket, "kill-server"], {
        stdout: "ignore", stderr: "ignore",
      }).exited;
    } finally {
      client.kill();
      client.terminal?.close();
      await client.exited;
      rmSync(dir, { recursive: true, force: true });
    }
  }
}, 10000);
