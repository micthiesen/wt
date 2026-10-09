import { describe, expect, spyOn, test } from "bun:test";
import { Effect } from "effect";

import type { RegistrySession } from "../../core/harness/claude/registry.ts";
import type { Worktree } from "../../core/types.ts";
import { run, sessionInfoFor } from "./fleet.ts";

const worktree: Worktree = {
  slug: "busy-codex",
  branch: "michael/busy-codex",
  path: "/tmp/busy-codex",
  stage: "busy-codex",
  isMain: false,
};
const registryEntry: RegistrySession = {
  pid: 42,
  sessionId: "session-1",
  cwd: worktree.path,
  name: worktree.slug,
  status: "busy",
  waitingFor: null,
  kind: "claude",
  entrypoint: "cli",
  startedAt: 1_000,
  updatedAt: 2_000,
};

describe("wt fleet session liveness", () => {
  test("reports a live Codex or OpenCode primary without inventing activity", () => {
    expect(
      sessionInfoFor(worktree, new Set([worktree.slug]), new Set(), []),
    ).toEqual({ alive: true, busy: null, last_activity: null });
  });

  test("ignores stale Claude activity when only another harness is live", () => {
    expect(
      sessionInfoFor(worktree, new Set([worktree.slug]), new Set(), [registryEntry]),
    ).toEqual({ alive: true, busy: null, last_activity: null });
  });

  test("does not infer liveness from an old registry entry", () => {
    expect(
      sessionInfoFor(worktree, new Set(), new Set(), [registryEntry]),
    ).toEqual({ alive: false, busy: null, last_activity: null });
  });

  test("does not borrow activity from a named session or another worktree", () => {
    expect(
      sessionInfoFor(
        worktree,
        new Set([worktree.slug]),
        new Set([worktree.slug]),
        [
          { ...registryEntry, name: "scratch" },
          { ...registryEntry, cwd: "/tmp/another-worktree" },
        ],
      ),
    ).toEqual({ alive: true, busy: null, last_activity: null });
  });

  test("keeps Claude registry detail when Claude owns the live primary", () => {
    expect(
      sessionInfoFor(
        worktree,
        new Set([worktree.slug]),
        new Set([worktree.slug]),
        [registryEntry],
      ),
    ).toEqual({
      alive: true,
      busy: true,
      last_activity: new Date(2_000).toISOString(),
    });
  });
});

describe("wt fleet arguments", () => {
  test("rejects trailing positional arguments before doing I/O", async () => {
    const error = spyOn(console, "error").mockImplementation(() => {});
    try {
      expect(await Effect.runPromise(run(["extra"]))).toBe(2);
      expect(error).toHaveBeenCalledWith(
        expect.stringContaining("unexpected argument: extra"),
      );
    } finally {
      error.mockRestore();
    }
  });
});
