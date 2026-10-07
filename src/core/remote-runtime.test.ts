import { describe, expect, test } from "bun:test";
import { chmodSync, mkdtempSync, rmSync, symlinkSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { Effect } from "effect";
import { runtimeHash as runtimeHashEffect, runtimePath } from "./remote-runtime.ts";
import { remoteWtCommand, setRemoteRuntimePath } from "./remote-protocol.ts";

const runtimeHash = (root: string, files: string[], version: string) => Effect.runSync(runtimeHashEffect(root, files, version));

describe("remote runtime identity", () => {
  test("reuses identical content but detects content, file set, mode, and build changes", () => {
    const root = mkdtempSync(join(tmpdir(), "wt-runtime-test-"));
    try {
      writeFileSync(join(root, "a"), "one");
      writeFileSync(join(root, "b"), "two");
      const original = runtimeHash(root, ["a", "b"], "build");
      utimesSync(join(root, "a"), new Date(0), new Date(0));
      expect(runtimeHash(root, ["b", "a"], "build")).toBe(original);
      expect(runtimeHash(root, ["a"], "build")).not.toBe(original);
      expect(runtimeHash(root, ["a", "b"], "next build")).not.toBe(original);
      chmodSync(join(root, "a"), 0o755);
      expect(runtimeHash(root, ["a", "b"], "build")).not.toBe(original);
      writeFileSync(join(root, "b"), "new");
      expect(runtimeHash(root, ["a", "b"], "build")).not.toBe(original);
      symlinkSync("a", join(root, "link"));
      expect(runtimeHash(root, ["link"], "build")).toHaveLength(64);
    } finally {
      rmSync(root, { recursive: true, force: true });
    }
  });

  test("rejects unsafe package identifiers", () => {
    expect(() => runtimePath("../escape")).toThrow("invalid runtime hash");
  });

  test("pins one endpoint without changing another endpoint or its configured path", () => {
    const remote = { host: "runtime-test", label: "worker", wtPath: "~/bin/wt" };
    const path = runtimePath("a".repeat(64));
    setRemoteRuntimePath(remote, path);
    expect(remoteWtCommand(remote, ["new", "task"])).toContain(".cache/wt/runtimes/");
    expect(remote.wtPath).toBe("~/bin/wt");
    expect(remoteWtCommand({ ...remote, wtPath: "~/other/wt" }, null)).toContain("other/wt");
  });
});
