import { expect, test } from "bun:test";
import type { RemoteWorktreeSummary } from "../core/remote-worktrees.ts";
import { consumeRemoteCreationSession, type RemoteCreation } from "./remote-creation.ts";

function creation(): RemoteCreation {
  return {
    remote: { host: "worker", label: "Worker", wtPath: "~/bin/wt" },
    hostKey: "worker", hostLabel: "Worker", input: "task", previousKeys: [],
    status: "creating", requestedHarness: "codex",
  };
}
const row = { hostKey: "worker", slug: "task" } as RemoteWorktreeSummary;

test("a creation request waits for success and a new checkout from the same host", () => {
  const pending = creation();
  expect(consumeRemoteCreationSession(pending, row)).toBeUndefined();
  pending.status = "ready";
  expect(consumeRemoteCreationSession(pending, undefined)).toBeUndefined();
  expect(consumeRemoteCreationSession(pending, { ...row, hostKey: "other" })).toBeUndefined();
  expect(pending.requestedHarness).toBe("codex");
  expect(consumeRemoteCreationSession(pending, row)).toBe("codex");
  expect(consumeRemoteCreationSession(pending, row)).toBeUndefined();
});

test("a pre-existing checkout cannot consume a new creation's session request", () => {
  const pending = creation();
  pending.status = "ready";
  pending.previousKeys = ["worker:task"];
  expect(consumeRemoteCreationSession(pending, row)).toBeUndefined();
  expect(pending.requestedHarness).toBe("codex");
});
