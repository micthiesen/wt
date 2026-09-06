import { afterEach, describe, expect, test } from "bun:test";
import { existsSync, mkdtempSync, readFileSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { inspectorSocketPath } from "../harness/claude/inject.ts";
import { wrapInnerArgs } from "./inner-process.ts";

const tempDirs: string[] = [];

afterEach(() => {
  for (const dir of tempDirs.splice(0)) {
    rmSync(dir, { recursive: true, force: true });
  }
});

async function runWrapped(kind: "shell" | "claude", message: string) {
  const dir = mkdtempSync(join(tmpdir(), "wt-stderr-wrapper-"));
  tempDirs.push(dir);
  const stderrPath = join(dir, "session.err");
  const proc = Bun.spawn(
    wrapInnerArgs({
      kind,
      stderrPath,
      innerArgs: ["bash", "-c", 'printf "%s" "$1" >&2', "_inner", message],
    }),
    { stdout: "pipe", stderr: "pipe" },
  );
  const [exitCode, stderr] = await Promise.all([
    proc.exited,
    new Response(proc.stderr).text(),
  ]);
  return { exitCode, stderr, stderrPath };
}

describe("tmux inner-process browser identity", () => {
  test("every session can invoke this checkout's wt launcher", async () => {
    for (const kind of ["shell", "claude"] as const) {
      const proc = Bun.spawn(
        wrapInnerArgs({
          kind,
          stderrPath: "/dev/null",
          innerArgs: ["sh", "-c", "command -v wt"],
        }),
        { stdout: "pipe", stderr: "ignore" },
      );
      const [exitCode, stdout] = await Promise.all([
        proc.exited,
        new Response(proc.stdout).text(),
      ]);
      expect(exitCode).toBe(0);
      expect(stdout.trim()).toEndWith("/bin/wt");
    }
  });

  test("the harness inherits its worktree's browser session name", async () => {
    const proc = Bun.spawn(
      wrapInnerArgs({
        kind: "claude",
        stderrPath: "/dev/null",
        innerArgs: ["printenv", "BROWSER_CONTROL_SESSION"],
        slug: "eng-1-slug",
      }),
      { stdout: "pipe", stderr: "ignore" },
    );
    const [, stdout] = await Promise.all([proc.exited, new Response(proc.stdout).text()]);
    expect(stdout.trim()).toBe("wt-eng-1-slug");
  });

  test("the harness knows which worktree's agent it is", async () => {
    // `WT_AGENT` is what makes an outgoing `wt manager send` stamp its
    // own sender — the prefix agents used to have to remember.
    const proc = Bun.spawn(
      wrapInnerArgs({
        kind: "claude",
        stderrPath: "/dev/null",
        innerArgs: ["printenv", "WT_AGENT"],
        slug: "eng-1-slug",
      }),
      { stdout: "pipe", stderr: "ignore" },
    );
    const [, stdout] = await Promise.all([proc.exited, new Response(proc.stdout).text()]);
    expect(stdout.trim()).toBe("eng-1-slug");
  });

  test("interactive harnesses do not inherit no-color flags", async () => {
    for (const kind of ["claude", "codex", "opencode"] as const) {
      const proc = Bun.spawn(
        wrapInnerArgs({
          kind,
          stderrPath: "/dev/null",
          innerArgs: ["sh", "-c", "printenv NO_COLOR NO_COLOUR"],
          slug: "eng-1-slug",
        }),
        {
          stdout: "pipe",
          stderr: "ignore",
          env: { ...process.env, NO_COLOR: "1", NO_COLOUR: "1" },
        },
      );
      const [, stdout] = await Promise.all([
        proc.exited,
        new Response(proc.stdout).text(),
      ]);
      expect(stdout.trim()).toBe("");
    }
  });

  test("a claude session does not inherit the caller's own Claude identity", async () => {
    // wt is usually run BY an agent, so its environment IS a Claude
    // session's. `CLAUDE_CODE_CHILD_SESSION` in particular makes the new
    // session stop writing a transcript — which wt reads for delivery
    // confirmation, status, summaries and the away feed. Started from a
    // shell it looked perfect; started by an agent it lost all of it.
    const proc = Bun.spawn(
      wrapInnerArgs({
        kind: "claude",
        stderrPath: "/dev/null",
        innerArgs: ["sh", "-c", "printenv CLAUDE_CODE_CHILD_SESSION CLAUDE_CODE_MESSAGING_SOCKET"],
        slug: "eng-1-slug",
        tmuxName: "eng-1-slug",
      }),
      {
        stdout: "pipe",
        stderr: "ignore",
        env: {
          ...process.env,
          CLAUDE_CODE_CHILD_SESSION: "1",
          CLAUDE_CODE_MESSAGING_SOCKET: "/tmp/some-other-session.sock",
        },
      },
    );
    const [, stdout] = await Promise.all([proc.exited, new Response(proc.stdout).text()]);
    expect(stdout.trim()).toBe("");
  });

  test("a human's shell in a worktree is not that worktree's agent", async () => {
    // Otherwise `wt manager send` typed by hand at an F10 shell would
    // arrive signed as the agent, and the manager would answer a person
    // as if it were coordinating a worker.
    const proc = Bun.spawn(
      wrapInnerArgs({
        kind: "shell",
        stderrPath: "/dev/null",
        innerArgs: ["printenv", "WT_AGENT"],
        slug: "eng-1-slug",
      }),
      { stdout: "pipe", stderr: "ignore" },
    );
    const [, stdout] = await Promise.all([proc.exited, new Response(proc.stdout).text()]);
    expect(stdout.trim()).toBe("");
  });

  test("a claude session is launched with its own inspector socket", async () => {
    const proc = Bun.spawn(
      wrapInnerArgs({
        kind: "claude",
        stderrPath: "/dev/null",
        innerArgs: ["printenv", "BUN_INSPECT"],
        slug: "eng-1-slug",
        tmuxName: "eng-1-slug",
      }),
      { stdout: "pipe", stderr: "ignore" },
    );
    const [, stdout] = await Promise.all([proc.exited, new Response(proc.stdout).text()]);
    expect(stdout.trim()).toBe(`ws+unix://${inspectorSocketPath("eng-1-slug")}`);
  });

  test("no tmux name, no inspector — the session still starts", async () => {
    // Delivery degrades to the terminal transport; a session that
    // cannot be addressed is still better than one that won't boot.
    const proc = Bun.spawn(
      wrapInnerArgs({
        kind: "claude",
        stderrPath: "/dev/null",
        innerArgs: ["printenv", "BUN_INSPECT"],
        slug: "eng-1-slug",
      }),
      { stdout: "pipe", stderr: "ignore" },
    );
    const [, stdout] = await Promise.all([proc.exited, new Response(proc.stdout).text()]);
    expect(stdout.trim()).toBe("");
  });

  test("no slug, no stamp — nothing inherits a stale identity", async () => {
    const proc = Bun.spawn(
      wrapInnerArgs({
        kind: "shell",
        stderrPath: "/dev/null",
        innerArgs: ["printenv", "BROWSER_CONTROL_SESSION"],
      }),
      { stdout: "pipe", stderr: "ignore" },
    );
    const [, stdout] = await Promise.all([proc.exited, new Response(proc.stdout).text()]);
    expect(stdout.trim()).toBe("");
  });
});

describe("tmux inner-process stderr routing", () => {
  test("shell prompts remain visible on the tmux PTY", async () => {
    const result = await runWrapped(
      "shell",
      "Do you want to continue? [Y/n]",
    );

    expect(result.exitCode).toBe(0);
    expect(result.stderr).toBe("Do you want to continue? [Y/n]");
    expect(existsSync(result.stderrPath)).toBe(false);
  });

  test("harness startup errors remain captured after the process exits", async () => {
    const result = await runWrapped("claude", "session id already exists");

    expect(result.exitCode).toBe(0);
    expect(result.stderr).toBe("");
    expect(readFileSync(result.stderrPath, "utf8")).toBe(
      "session id already exists",
    );
  });
});

test("persistent sessions do not inherit presentation workspace identity", async () => {
  const proc = Bun.spawn(wrapInnerArgs({
    kind: "shell", stderrPath: "/dev/null", innerArgs: ["printenv", "WT_WORKSPACE_SOCKET", "WT_WORKSPACE"],
  }), { stdout: "pipe", stderr: "ignore", env: { ...process.env, WT_WORKSPACE_SOCKET: "old-view", WT_WORKSPACE: "on" } });
  const [, stdout] = await Promise.all([proc.exited, new Response(proc.stdout).text()]);
  expect(stdout.trim()).toBe("");
});
