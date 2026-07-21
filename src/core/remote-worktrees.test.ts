import { describe, expect, test } from "bun:test";

import { parseRemoteWorktrees } from "./remote-worktrees.ts";

describe("parseRemoteWorktrees", () => {
  test("normalizes wt ls JSON with host identity", () => {
    const rows = parseRemoteWorktrees(JSON.stringify([{
      slug: "remote-test",
      branch: "alex/remote-test",
      path: "/home/alex/dev/client-app-worktrees/remote-test",
      stage: "alex-123",
      exists: true,
      status: "busy",
      status_label: "init: pnpm install",
      status_age: "2m",
      dirty: false,
      unpushed: 2,
      linear_url: null,
    }]), "cachy");
    expect(rows[0]).toEqual({
      hostLabel: "cachy",
      slug: "remote-test",
      branch: "alex/remote-test",
      path: "/home/alex/dev/client-app-worktrees/remote-test",
      stage: "alex-123",
      exists: true,
      status: "busy",
      statusLabel: "init: pnpm install",
      statusAge: "2m",
      dirty: false,
      unpushed: 2,
      linearUrl: null,
    });
  });

  test("defaults missing unpushed metadata for older remote binaries", () => {
    const [row] = parseRemoteWorktrees(JSON.stringify([{
      slug: "x", branch: "x", path: "/x", stage: "x", exists: true,
      status: "clean", status_label: "clean", dirty: false,
    }]), "cachy");
    expect(row?.unpushed).toBe(0);
  });

  test("rejects malformed status values", () => {
    expect(() => parseRemoteWorktrees(JSON.stringify([{
      slug: "x", branch: "x", path: "/x", stage: "x", exists: true,
      status: "wat", status_label: "wat", dirty: false,
    }]), "cachy")).toThrow("status is invalid");
  });
});
