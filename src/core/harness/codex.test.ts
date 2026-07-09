import { mkdtempSync, statSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { describe, expect, test } from "bun:test";

import { readCodexTail } from "./codex/harness.ts";

function event(type: string, timestamp: string): string {
  return JSON.stringify({
    timestamp,
    type: "event_msg",
    payload: { type, turn_id: "turn-1" },
  });
}

function writeRollout(lines: string[]): { path: string; mtimeMs: number; size: number } {
  const dir = mkdtempSync(join(tmpdir(), "wt-codex-tail-"));
  const path = join(dir, "rollout.jsonl");
  writeFileSync(path, `${lines.join("\n")}\n`);
  const st = statSync(path);
  return { path, mtimeMs: st.mtimeMs, size: st.size };
}

describe("readCodexTail", () => {
  test("detects an active turn when task_started is pushed behind a large line", () => {
    const rollout = writeRollout([
      event("task_started", "2026-07-08T12:00:00.000Z"),
      JSON.stringify({
        timestamp: "2026-07-08T12:00:01.000Z",
        type: "response_item",
        payload: { type: "reasoning", encrypted_content: "x".repeat(96 * 1024) },
      }),
    ]);

    expect(readCodexTail(rollout.path, rollout.mtimeMs, rollout.size)).toEqual({
      tailClosedCleanly: false,
      lastEventMs: Date.parse("2026-07-08T12:00:00.000Z"),
    });
  });

  test("uses the latest task lifecycle event when the turn has completed", () => {
    const rollout = writeRollout([
      event("task_started", "2026-07-08T12:00:00.000Z"),
      JSON.stringify({
        timestamp: "2026-07-08T12:00:01.000Z",
        type: "response_item",
        payload: { type: "reasoning", encrypted_content: "x".repeat(96 * 1024) },
      }),
      event("task_complete", "2026-07-08T12:00:02.000Z"),
    ]);

    expect(readCodexTail(rollout.path, rollout.mtimeMs, rollout.size)).toEqual({
      tailClosedCleanly: true,
      lastEventMs: Date.parse("2026-07-08T12:00:02.000Z"),
    });
  });
});
