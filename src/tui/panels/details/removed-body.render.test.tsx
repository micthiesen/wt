import { expect, test } from "bun:test";
import { act } from "react";
import { testRender } from "@opentui/react/test-utils";

import type { RemovedWorktree } from "../../../core/wtstate.ts";
import { issueStatusBadge } from "../../badges.ts";
import { RemovedBody } from "./removed-body.tsx";

const entry: RemovedWorktree = {
  slug: "coz-1445-systematic-toucan",
  branch: "michael/coz-1445-systematic-toucan",
  title: "Pilot guest admission on staff-selected meetings",
  issueId: "COZ-1777",
  githubIssue: 2116,
  removedAt: "2026-09-23T18:00:00Z",
  prNumber: 2150,
  prState: "MERGED",
};

test("removed details reuse the live issue line with override, status and GitHub relationship", async () => {
  const setup = await testRender(<RemovedBody entry={entry} width={70} issueStatus="In Review" />, { width: 70, height: 20 });
  try {
    await setup.flush();
    const lines = setup.captureCharFrame().split("\n");
    const issue = lines.find((line) => line.includes("#COZ-1777"));
    expect(issue).toContain(`${issueStatusBadge("In Review").glyph}  #COZ-1777 ← #2116 · In Review`);
    expect(lines.some((line) => line.includes("#COZ-1445"))).toBe(false);
    expect(lines.some((line) => line.includes("#2150") && line.includes("merged"))).toBe(true);
  } finally {
    act(() => setup.renderer.destroy());
  }
});

test("an explicit tracker unlink leaves only the recorded GitHub issue", async () => {
  const setup = await testRender(
    <RemovedBody entry={{ ...entry, issueId: "" }} width={70} issueStatus="In Review" />,
    { width: 70, height: 20 },
  );
  try {
    await setup.flush();
    const lines = setup.captureCharFrame().split("\n");
    const issue = lines.find((line) => line.includes("#2116"));
    expect(issue).toContain("issue #2116");
    expect(lines.some((line) => line.includes("#COZ-1777") || line.includes("#COZ-1445"))).toBe(false);
  } finally {
    act(() => setup.renderer.destroy());
  }
});
