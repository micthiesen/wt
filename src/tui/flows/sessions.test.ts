import { describe, expect, test } from "bun:test";

import type { HarnessSession } from "../../core/harness/index.ts";

import { slotSessionResumeTarget } from "./sessions.ts";

function session(sessionId: string, lastActiveMs: number | null): HarnessSession {
  return {
    displayName: sessionId,
    sessionId,
    tmuxSessionName: "slot-codex",
    lastActiveMs,
    isLive: false,
    extras: {
      managedName: null,
      derivedState: null,
      queued: 0,
    },
  };
}

describe("slotSessionResumeTarget", () => {
  test("does not resume multi-slot harnesses", () => {
    expect(
      slotSessionResumeTarget({ singleSlot: false }, false, [
        session("existing", 200),
      ]),
    ).toEqual({ resumeSessionId: null, freshSlot: false });
  });

  test("attaches to a live single-slot harness without resume argv", () => {
    expect(
      slotSessionResumeTarget({ singleSlot: true }, true, [
        session("existing", 200),
      ]),
    ).toEqual({ resumeSessionId: null, freshSlot: false });
  });

  test("resumes the newest discovered single-slot session when closed", () => {
    expect(
      slotSessionResumeTarget({ singleSlot: true }, false, [
        session("older", 100),
        session("newer", 300),
        session("unknown-time", null),
      ]),
    ).toEqual({ resumeSessionId: "newer", freshSlot: true });
  });

  test("starts fresh when a closed single-slot harness has no history", () => {
    expect(slotSessionResumeTarget({ singleSlot: true }, false, [])).toEqual({
      resumeSessionId: null,
      freshSlot: false,
    });
  });
});
