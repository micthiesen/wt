import { afterEach, beforeEach, describe, expect, test } from "bun:test";
import { mkdtempSync, mkdirSync, rmSync, readFileSync, renameSync, writeFileSync } from "node:fs";
import { Effect } from "effect";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  __setLedgerPathForTests,
  breakerState,
  bumpBreaker,
  cancelAutomationFires,
  dropFires,
  hasHandledFire,
  lastDispatchAt,
  markFiresDelivered,
  markFiresDispatched,
  reconcileDispatchedFires,
  resetBreaker,
  tripBreaker,
} from "./automations.ts";

let dir: string;

beforeEach(() => {
  dir = mkdtempSync(join(tmpdir(), "wt-auto-ledger-"));
  __setLedgerPathForTests(join(dir, "automations.json"));
});

afterEach(() => {
  rmSync(dir, { recursive: true, force: true });
});

describe("fire ledger", () => {
  test("failed dispatch persistence refuses launch and forgets its memory claim", () => {
    // Block only the write, after the lock and a valid ledger read succeed.
    mkdirSync(join(dir, `automations.json.${process.pid}.tmp`));
    let launched = false;
    expect(() => {
      if (markFiresDispatched(["pending"], "fix", "a")) launched = true;
    }).toThrow();
    expect(launched).toBe(false);
    expect(hasHandledFire("pending")).toBe(false);
    expect(lastDispatchAt("fix", "a")).toBeNull();
  });
  test("cancelled pending fires survive reload without changing running fires or cooldowns", async () => {
    markFiresDispatched(["running"], "fix", "a");
    const before = lastDispatchAt("fix", "a");
    await Effect.runPromise(cancelAutomationFires(["pending", "running"]));
    const stored = JSON.parse(readFileSync(join(dir, "automations.json"), "utf8"));
    expect(stored.fired.pending.state).toBe("cancelled");
    expect(stored.fired.running.state).toBe("dispatched");
    __setLedgerPathForTests(join(dir, "automations.json"));
    expect(hasHandledFire("pending")).toBe(true);
    expect(hasHandledFire("new-instance")).toBe(false);
    expect(lastDispatchAt("fix", "a")).toBe(before);
    expect(breakerState("fix", "a").count).toBe(0);
    reconcileDispatchedFires(() => false);
    expect(hasHandledFire("pending")).toBe(true);
  });

  test("failed cancellation writes roll back memory and report failure", async () => {
    const obstacle = join(dir, "not-a-directory");
    writeFileSync(obstacle, "fixture");
    __setLedgerPathForTests(join(obstacle, "automations.json"));
    const result = await Effect.runPromise(Effect.result(cancelAutomationFires(["pending"])));
    expect(result._tag).toBe("Failure");
    expect(hasHandledFire("pending")).toBe(false);
  });

  test("a warmed reader observes an atomic external cancellation without a reset", () => {
    markFiresDispatched(["ours"], "fix", "a");
    expect(hasHandledFire("external")).toBe(false);
    const file = join(dir, "automations.json");
    const stored = JSON.parse(readFileSync(file, "utf8"));
    stored.fired.external = { state: "cancelled", at: Date.now(), ruleId: "", slug: "" };
    const replacement = join(dir, "replacement.json");
    writeFileSync(replacement, JSON.stringify(stored));
    renameSync(replacement, file);
    expect(hasHandledFire("external")).toBe(true);
    bumpBreaker("fix", "a");
    expect(JSON.parse(readFileSync(file, "utf8")).fired.external.state).toBe("cancelled");
  });

  test("cancellation wins a stale dispatch attempt and cannot be delivered or dropped", async () => {
    expect(hasHandledFire("cancelled")).toBe(false);
    await Effect.runPromise(cancelAutomationFires(["cancelled"]));
    expect(markFiresDispatched(["cancelled"], "fix", "a")).toBe(false);
    expect(lastDispatchAt("fix", "a")).toBeNull();
    markFiresDelivered(["cancelled"]);
    dropFires(["cancelled"]);
    expect(JSON.parse(readFileSync(join(dir, "automations.json"), "utf8")).fired.cancelled.state).toBe("cancelled");
    expect(markFiresDispatched(["cancelled", "fresh"], "fix", "a")).toBe(true);
    const stored = JSON.parse(readFileSync(join(dir, "automations.json"), "utf8"));
    expect(stored.fired.cancelled.state).toBe("cancelled");
    expect(stored.fired.fresh.state).toBe("dispatched");
  });

  test("independent warmed writers preserve cancellations and concurrent breaker increments", async () => {
    const file = join(dir, "automations.json");
    markFiresDispatched(["existing"], "fix", "a");
    const moduleUrl = new URL("./automations.ts", import.meta.url).href;
    const children: Bun.Subprocess<"pipe", "pipe", "pipe">[] = [];
    const start = async (key: string) => {
      const child = Bun.spawn(["bun", "-e", `
        import { Effect } from "effect";
        import { __setLedgerPathForTests, hasHandledFire, cancelAutomationFires, bumpBreaker } from ${JSON.stringify(moduleUrl)};
        __setLedgerPathForTests(${JSON.stringify(file)});
        hasHandledFire("warm-cache");
        console.log("ready");
        await Bun.stdin.text();
        await Effect.runPromise(cancelAutomationFires([${JSON.stringify(key)}]));
        for (let i = 0; i < 30; i++) bumpBreaker("concurrent", "a");
      `], {
        cwd: join(import.meta.dir, "../.."),
        // Bun 1.4.1 children can miss env set by the test preload. Pin the
        // fixture explicitly, never fall through to the machine's config.
        env: { ...process.env, WT_CONFIG: join(import.meta.dir, "../../test/config.toml"), WT_REPO_CONFIG: "", BUN_INSPECT: "" },
        stdin: "pipe", stdout: "pipe", stderr: "pipe",
      });
      children.push(child);
      const reader = child.stdout.getReader();
      let output = "";
      try {
        while (!output.includes("\n")) {
          const chunk = await reader.read();
          if (chunk.done) throw new Error(`ledger child exited ${await child.exited}: ${await new Response(child.stderr).text()}`);
          output += new TextDecoder().decode(chunk.value);
        }
      } finally { reader.releaseLock(); }
      expect(output).toContain("ready");
      return child;
    };
    try {
      await Promise.all([start("external-a"), start("external-b")]);
      for (const child of children) child.stdin.end();
      for (const child of children) {
        expect(await child.exited, await new Response(child.stderr).text()).toBe(0);
      }
    } finally {
      for (const child of children) child.kill();
      await Promise.all(children.map((child) => child.exited));
    }
    expect(hasHandledFire("external-a")).toBe(true);
    expect(hasHandledFire("external-b")).toBe(true);
    expect(hasHandledFire("existing")).toBe(true);
    expect(breakerState("concurrent", "a").count).toBe(60);
  });

  test("dispatched keys count as handled and persist across a reload", () => {
    expect(hasHandledFire("ci:a:sha1")).toBe(false);
    markFiresDispatched(["ci:a:sha1", "ci:a:sha2"], "fix-ci", "a");
    expect(hasHandledFire("ci:a:sha1")).toBe(true);
    expect(hasHandledFire("ci:a:sha2")).toBe(true);
    // Reload from disk (fresh singleton, same file).
    __setLedgerPathForTests(join(dir, "automations.json"));
    expect(hasHandledFire("ci:a:sha1")).toBe(true);
    expect(lastDispatchAt("fix-ci", "a")).not.toBeNull();
  });

  test("dropFires un-consumes declined dispatches so the condition can re-fire", () => {
    markFiresDispatched(["ci:a:sha1"], "fix-ci", "a");
    expect(hasHandledFire("ci:a:sha1")).toBe(true);
    dropFires(["ci:a:sha1", "never-recorded"]);
    expect(hasHandledFire("ci:a:sha1")).toBe(false);
  });

  test("boot reconcile flips matched dispatches and drops unmatched ones", () => {
    markFiresDispatched(["ci:a:sha1"], "fix-ci", "a");
    markFiresDispatched(["ci:b:sha9"], "fix-ci", "b");
    markFiresDelivered(["ci:b:sha9"]);
    markFiresDispatched(["rabbit:c:sha3"], "auto-rabbit", "c");
    // A run exists for the ci:a key (crash after launch) but not for
    // the rabbit key (crash before launch).
    const dropped = reconcileDispatchedFires((k) => k === "ci:a:sha1");
    expect(dropped).toBe(1);
    expect(hasHandledFire("ci:a:sha1")).toBe(true);
    expect(hasHandledFire("ci:b:sha9")).toBe(true); // delivered untouched
    expect(hasHandledFire("rabbit:c:sha3")).toBe(false); // re-fires
  });
});

describe("circuit breaker", () => {
  test("counts consecutive dispatches, trips, and resets on clear", () => {
    expect(breakerState("fix-ci", "a")).toEqual({ count: 0, trippedAt: null, updatedAt: 0 });
    expect(bumpBreaker("fix-ci", "a")).toBe(1);
    expect(bumpBreaker("fix-ci", "a")).toBe(2);
    expect(breakerState("fix-ci", "a").updatedAt).toBeGreaterThan(0);
    tripBreaker("fix-ci", "a");
    expect(breakerState("fix-ci", "a").trippedAt).not.toBeNull();
    // Condition observed clear → full reset.
    resetBreaker("fix-ci", "a");
    expect(breakerState("fix-ci", "a")).toEqual({ count: 0, trippedAt: null, updatedAt: 0 });
  });

  test("breaker state is per (rule, slug)", () => {
    bumpBreaker("fix-ci", "a");
    expect(breakerState("fix-ci", "b").count).toBe(0);
    expect(breakerState("auto-rabbit", "a").count).toBe(0);
  });
});
