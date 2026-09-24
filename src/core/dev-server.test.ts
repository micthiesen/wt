import { describe, expect, test } from "bun:test";
import { Effect } from "effect";
import { once } from "node:events";
import net from "node:net";

import { run } from "./proc.ts";

import { existsSync, mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import {
  decideDevSlot,
  DEV_SERVER_STOPPED,
  DevResetStopFailedError,
  devServerCrashSummary,
  probePort,
  readDevWaiters,
  supervisorScript,
  type DevSlotHolder,
} from "./dev-server.ts";

test("failed reset stop reports unknown external state, not an untouched live stack", () => {
  const message = new DevResetStopFailedError("broken-stack").message;
  expect(message).toContain("external resources are unconfirmed");
  expect(message).toContain("reset_command was not run");
});

/** Hold the event loop hostage the way a heavy sync render does. */
function blockLoop(ms: number): void {
  const end = Date.now() + ms;
  while (Date.now() < end) {
    // spin
  }
}

async function withListener<T>(fn: (port: number) => Promise<T>): Promise<T> {
  const server = net.createServer(() => {});
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const { port } = server.address() as net.AddressInfo;
  try {
    return await fn(port);
  } finally {
    server.close();
  }
}

/** A port nothing is listening on: bind one, then release it. */
async function closedPort(): Promise<number> {
  const server = net.createServer(() => {});
  server.listen(0, "127.0.0.1");
  await once(server, "listening");
  const { port } = server.address() as net.AddressInfo;
  server.close();
  await once(server, "close");
  return port;
}

describe("probePort", () => {
  test("reports a listening port", async () => {
    await withListener(async (port) => {
      expect(await Effect.runPromise(probePort(port))).toBe("listening");
    });
  });

  test("reports a closed port free (refused, not timed out)", async () => {
    expect(await Effect.runPromise(probePort(await closedPort()))).toBe("free");
  });

  // The bug this whole three-state probe exists for: libuv runs timers
  // before poll, so an event loop blocked past the deadline fires the
  // timeout callback ahead of a `connect` that already succeeded. The
  // old boolean probe returned false there — a live dev server read as
  // dead and the bolt vanished from the row.
  test("never reports a listening port free when the loop stalls past the deadline", async () => {
    await withListener(async (port) => {
      const probe = Effect.runPromise(probePort(port, 100));
      blockLoop(300);
      expect(await probe).not.toBe("free");
    });
  });

  test("retry resolves a stall-induced inconclusive probe", async () => {
    await withListener(async (port) => {
      const probe = Effect.runPromise(probePort(port, 100));
      blockLoop(300);
      // First attempt times out mid-stall; the second runs on a loop
      // that is moving again and gets the real answer.
      expect(await probe).toBe("listening");
    });
  });
});

// ---------------------------------------------------------------------------
// Concurrency slots
// ---------------------------------------------------------------------------

/**
 * A pid that cannot exist. macOS caps pids at 99999 (`kern.maxproc`
 * ceiling), so this is a deterministic dead process — better than
 * spawning something and reusing its pid, which a busy machine could
 * recycle inside the test.
 */
const DEAD_PID = 999_999;

describe("decideDevSlot", () => {
  const up = (slug: string): DevSlotHolder => ({ slug, state: "up" });

  test("no cap configured means every start proceeds", () => {
    const d = decideDevSlot("mine", [up("a"), up("b"), up("c")], null);
    expect(d.ok).toBe(true);
    expect(d.free).toBeNull();
  });

  test("refuses once the cap is reached", () => {
    const d = decideDevSlot("mine", [up("a"), up("b")], 2);
    expect(d.ok).toBe(false);
    expect(d.free).toBe(0);
    expect(d.holders.map((h) => h.slug)).toEqual(["a", "b"]);
  });

  // Start is also restart. A worktree already holding a slot is not
  // asking for a second one, and refusing it would make a full fleet
  // unable to pick up a config edit.
  test("the asking slug never counts against itself", () => {
    const d = decideDevSlot("mine", [up("a"), up("mine")], 2);
    expect(d.ok).toBe(true);
    expect(d.free).toBe(1);
    expect(d.holders.map((h) => h.slug)).toEqual(["a"]);
  });

  // A parked supervisor is a holder like any other: the containers and
  // tunnels its command created outside its own process tree are still
  // up, and those are what the cap rations.
  test("a crashed server still holds its slot", () => {
    const d = decideDevSlot("mine", [{ slug: "a", state: "crashed" }], 1);
    expect(d.ok).toBe(false);
    expect(d.holders[0]!.state).toBe("crashed");
  });

  test("free never goes negative when the fleet is over the cap", () => {
    const d = decideDevSlot("mine", [up("a"), up("b"), up("c")], 2);
    expect(d.free).toBe(0);
  });
});

describe("readDevWaiters", () => {
  function queueDir(): string {
    const dir = mkdtempSync(join(tmpdir(), "wt-devqueue-"));
    return dir;
  }
  const write = (dir: string, slug: string, rec: unknown) =>
    writeFileSync(join(dir, `${slug}.json`), JSON.stringify(rec));

  test("returns live waiters oldest first", () => {
    const dir = queueDir();
    write(dir, "later", { pid: process.pid, since: 2000 });
    write(dir, "earlier", { pid: process.pid, since: 1000 });
    expect(readDevWaiters(dir).map((w) => w.slug)).toEqual(["earlier", "later"]);
  });

  // The self-expiring half. Nothing ever has to remember to leave the
  // queue: a waiter killed outright leaves a file that the next reader
  // discards, which is what keeps a slot ledger from drifting.
  test("drops and deletes the entry of a process that is gone", () => {
    const dir = queueDir();
    write(dir, "dead", { pid: DEAD_PID, since: 1000 });
    write(dir, "alive", { pid: process.pid, since: 2000 });
    expect(readDevWaiters(dir).map((w) => w.slug)).toEqual(["alive"]);
    expect(existsSync(join(dir, "dead.json"))).toBe(false);
    expect(existsSync(join(dir, "alive.json"))).toBe(true);
  });

  test("discards torn or hand-edited files rather than throwing", () => {
    const dir = queueDir();
    writeFileSync(join(dir, "torn.json"), "{not json");
    write(dir, "nopid", { since: 1000 });
    write(dir, "good", { pid: process.pid, since: 1000 });
    expect(readDevWaiters(dir).map((w) => w.slug)).toEqual(["good"]);
    expect(existsSync(join(dir, "torn.json"))).toBe(false);
    expect(existsSync(join(dir, "nopid.json"))).toBe(false);
  });

  test("an absent queue directory is an empty queue, not an error", () => {
    expect(readDevWaiters(join(tmpdir(), "wt-devqueue-does-not-exist"))).toEqual([]);
  });
});


describe("dev-queue priority", () => {
  const dir = () => mkdtempSync(join(tmpdir(), "wt-devprio-"));
  const write = (d: string, slug: string, rec: unknown) =>
    writeFileSync(join(d, `${slug}.json`), JSON.stringify(rec));

  // The whole point: a promotion has to change who the NEXT free slot
  // goes to, without the promoted agent doing anything. Its own poll
  // re-reads the queue and finds itself at the front.
  test("a promoted waiter sorts ahead of everyone who arrived earlier", () => {
    const d = dir();
    write(d, "early-a", { pid: process.pid, since: 1000 });
    write(d, "early-b", { pid: process.pid, since: 2000 });
    write(d, "urgent", { pid: process.pid, since: 9000, priority: 1 });
    expect(readDevWaiters(d).map((w) => w.slug)).toEqual(["urgent", "early-a", "early-b"]);
  });

  test("arrival still orders within a tier", () => {
    const d = dir();
    write(d, "second", { pid: process.pid, since: 2000, priority: 1 });
    write(d, "first", { pid: process.pid, since: 1000, priority: 1 });
    write(d, "ordinary", { pid: process.pid, since: 500 });
    expect(readDevWaiters(d).map((w) => w.slug)).toEqual(["first", "second", "ordinary"]);
  });

  test("an absent or junk priority is the ordinary tier, not a crash", () => {
    const d = dir();
    write(d, "none", { pid: process.pid, since: 1000 });
    write(d, "junk", { pid: process.pid, since: 2000, priority: "high" });
    expect(readDevWaiters(d).map((w) => w.priority)).toEqual([0, 0]);
  });

  // Self-expiry has to survive the new field: a promoted waiter that
  // dies must not keep steering the queue from a file nobody prunes.
  test("a dead promoted waiter is pruned like any other", () => {
    const d = dir();
    write(d, "dead-vip", { pid: DEAD_PID, since: 1000, priority: 1 });
    write(d, "alive", { pid: process.pid, since: 2000 });
    expect(readDevWaiters(d).map((w) => w.slug)).toEqual(["alive"]);
    expect(existsSync(join(d, "dead-vip.json"))).toBe(false);
  });
});


describe("dev-server readiness", () => {
  // The report that started this: a dev command that brings up a
  // database and THEN applies migrations. `supabase start` succeeds,
  // the port opens, the URL works — and the migration phase throws a
  // minute later, in the background, after `wt dev start` has already
  // exited 0. Every cheap signal says ready and none of them mean it.
  test("a listening port is not readiness, so DevServerStatus keeps them separate", () => {
    // `running` is the port; `rebasedSince` and the health command are
    // the two independent answers to "but is it USABLE". Pinned as a
    // shape test because the whole defect was one standing in for the
    // others.
    const keys = Object.keys(DEV_SERVER_STOPPED);
    expect(keys).toContain("running");
    expect(keys).toContain("rebasedSince");
  });

  // Absent must never read as fine. A server started by a wt that
  // predates the anchor has nothing to compare against, and claiming
  // freshness for it would be the same silent lie the field exists to
  // break.
  test("no anchor means unknown, not fresh", () => {
    expect(DEV_SERVER_STOPPED.rebasedSince).toBeNull();
  });
});

describe("devServerCrashSummary", () => {
  test("returns the last application error before the supervisor epilogue", () => {
    const output = [
      "Starting local services",
      "\u001b[31mCannot connect to /Users/alex/.orbstack/run/docker.sock\u001b[0m",
      "wt: dev server exited (1) — restarting in 2s",
      "wt: dev server crashed 3 times in a row (last exit 1) — giving up.",
      'wt: fix the cause, then start it again from the ! menu or "wt dev start".',
    ].join("\n");
    expect(devServerCrashSummary(output)).toBe(
      "Cannot connect to /Users/alex/.orbstack/run/docker.sock",
    );
  });

  test("returns null when the pane contains only supervisor output", () => {
    expect(
      devServerCrashSummary(
        "wt: dev server crashed 3 times in a row (last exit 1) — giving up.\n",
      ),
    ).toBeNull();
  });

  test("flattens control noise and caps the feed payload", () => {
    const summary = devServerCrashSummary(`error:\t${"x".repeat(300)}\x07`);
    expect(summary).not.toContain("\x07");
    expect(summary!.length).toBe(180);
    expect(summary!.endsWith("…")).toBe(true);
  });
});

describe("supervisorScript", () => {
  const script = supervisorScript("demo-slug", "pnpm dev --port {{port}}", 8123);

  test("is valid POSIX sh", async () => {
    // A syntax error here breaks EVERY dev server on the machine, and it
    // would surface as "the pane exits instantly" rather than as
    // anything naming this file. `sh -n` parses without executing.
    const f = join(tmpdir(), `wt-supervisor-${process.pid}.sh`);
    writeFileSync(f, script);
    try {
      // Explicit cwd: `run` defaults to `config.paths.mainClone`, and CI
      // points that at a synthetic path that does not exist, where
      // Bun.spawn fails on the cwd before `sh` ever runs. Passing 0
      // locally and failing in CI is exactly the shape that invariant
      // exists to prevent.
      const r = await Effect.runPromise(run(["sh", "-n", f], { cwd: tmpdir() }));
      expect(`${r.exitCode} ${r.stderr}`.trim()).toBe("0");
    } finally {
      rmSync(f, { force: true });
    }
  });

  test("captures the failure signature before wt echoes anything of its own", () => {
    // The ordering bug this guards: if the supervisor prints its restart
    // notice first, the next pass compares wt's own line with itself,
    // every failure looks deterministic, and the early-park fires on a
    // genuine flake.
    const sigAt = script.indexOf("sig=\"$code|");
    const echoAt = script.indexOf("wt: dev server exited");
    expect(sigAt).toBeGreaterThan(-1);
    expect(echoAt).toBeGreaterThan(-1);
    expect(sigAt).toBeLessThan(echoAt);
  });

  test("addresses the pane with a window-qualified target", () => {
    // `-t =name` is a SESSION target and capture-pane rejects it with
    // "can't find pane". It fails silently in the supervisor (stderr is
    // dropped), so the signature was always empty, every failure looked
    // unknown, and the early-park could never fire — a feature that
    // reads as shipped and does nothing. paneTarget's trailing colon is
    // the whole difference.
    expect(script).toContain("capture-pane -p -t '=demo-slug-dev:'");
  });

  test("hands off to _dev-giveup before exiting the park branch", () => {
    const handoff = script.indexOf("_dev-giveup");
    expect(handoff).toBeGreaterThan(-1);
    // Must come after the marker write, so a crash-log capture that
    // races the marker still sees `crashed`.
    expect(script.indexOf("printf crashed")).toBeLessThan(handoff);
    // `|| true` so cleanup trouble can never mask the park itself.
    expect(script.slice(handoff, handoff + 200)).toContain("|| true");
  });

  test("a long healthy run clears the deterministic-failure memory", () => {
    // Otherwise a failure from before a successful multi-hour run could
    // pair with an unrelated one after it and park on first failure.
    const elseBranch = script.slice(script.indexOf("    fails=0"));
    expect(elseBranch.slice(0, 120)).toContain("prev_sig=''");
  });
});
