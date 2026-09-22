import { describe, expect, test } from "bun:test";
import { readFileSync } from "node:fs";
import { join } from "node:path";
import { Effect } from "effect";

import { config } from "./config.ts";
import { createLogger, flushLogger } from "./logger.ts";

/**
 * Writes are chained asynchronously, so anything that logs and then exits
 * loses the line unless it flushes first. The lines that matters for are
 * precisely the ones explaining why a process is about to disappear — the
 * events daemon's self-restart notice was lost exactly this way, leaving a
 * pid change with a clean gap in the log.
 */
describe("flushLogger", () => {
  test("structured duration fields are rounded without changing other numeric metadata", async () => {
    const marker = `logger-durations-${process.pid}-${Date.now()}`;
    createLogger("[logger-test]").debug(marker, { elapsedMs: 799.797119140625, duration_ms: 23.999, durationSeconds: 1.234, ratio: 0.123456 });
    await Effect.runPromise(flushLogger);
    const day = new Date().toISOString().slice(0, 10);
    const body = readFileSync(join(config.paths.appLogDir, `wt-${day}.log`), "utf8");
    expect(body).toContain(`${marker} {"elapsedMs":800,"duration_ms":24,"durationSeconds":1.2,"ratio":0.123456}`);
  });
  test("a line logged immediately before it is on disk after it", async () => {
    const marker = `flushLog-probe-${process.pid}-${performance.now()}`;
    createLogger("[logger-test]").debug(marker);
    await Effect.runPromise(flushLogger);
    const day = new Date().toISOString().slice(0, 10);
    const body = readFileSync(join(config.paths.appLogDir, `wt-${day}.log`), "utf8");
    expect(body).toContain(marker);
  });

  test("it resolves rather than throwing when nothing is queued", async () => {
    await Effect.runPromise(flushLogger);
    await Effect.runPromise(flushLogger);
  });

  test("concurrent producers retain queue order through a flush barrier", async () => {
    const prefix = `logger-order-${process.pid}-${performance.now()}`;
    const logger = createLogger("[logger-test]");
    for (let index = 0; index < 20; index++) logger.debug(`${prefix}-${index}`);
    await Effect.runPromise(flushLogger);
    const day = new Date().toISOString().slice(0, 10);
    const body = readFileSync(join(config.paths.appLogDir, `wt-${day}.log`), "utf8");
    let previous = -1;
    for (let index = 0; index < 20; index++) {
      const at = body.indexOf(`${prefix}-${index}`);
      expect(at).toBeGreaterThan(previous);
      previous = at;
    }
  });
});
