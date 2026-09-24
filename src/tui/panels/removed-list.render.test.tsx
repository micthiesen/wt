import { expect, test } from "bun:test";
import { act } from "react";
import { testRender } from "@opentui/react/test-utils";

import type { RemovedWorktree } from "../../core/wtstate.ts";
import { NF } from "../icons.ts";
import { RemovedList } from "./removed-list.tsx";

const entries: RemovedWorktree[] = [
  {
    slug: "coz-1445-systematic-toucan",
    branch: "michael/coz-1445-systematic-toucan",
    title: "Fix a very long saved chat drafts title with attachments and replies",
    removedAt: "2026-09-23T18:00:00Z",
    prNumber: 2150,
    prState: "MERGED",
  },
  {
    slug: "coz-2517-clever-ibis",
    branch: "michael/coz-2517-clever-ibis",
    title: "Improve frontend screenshots",
    removedAt: "2026-09-23T17:00:00Z",
    prNumber: 2146,
    prState: "OPEN",
  },
];

test("removed rows stay one TUI line with aligned trailing slots", async () => {
  for (const width of [30, 35, 45, 60]) {
    const setup = await testRender(
      <RemovedList
        entries={entries}
        selectedIndex={0}
        width={width}
        issueStatuses={{ "COZ-1445": "In Review", "COZ-2517": "Completed" }}
      />,
      { width, height: 20 },
    );
    try {
      await setup.flush();
      const lines = setup.captureCharFrame().split("\n");
      const first = lines.find((line) => line.includes("1445:"));
      const second = lines.find((line) => line.includes("2517:"));
      expect(first).toBeDefined();
      expect(second).toBeDefined();
      expect(first).toContain(NF.prMerged);
      expect(second).toContain(NF.prOpen);
      expect(Bun.stringWidth(first!.slice(0, first!.lastIndexOf(NF.prMerged)))).toBe(
        Bun.stringWidth(second!.slice(0, second!.lastIndexOf(NF.prOpen))),
      );
      expect(setup.renderer.root.findDescendantById(`removed:${entries[0]!.slug}`)?.height).toBe(1);
      expect(setup.renderer.root.findDescendantById(`removed:${entries[1]!.slug}`)?.height).toBe(1);
      expect(lines.filter((line) => line.includes("1445:"))).toHaveLength(1);
      expect(lines.filter((line) => line.includes("2517:"))).toHaveLength(1);
    } finally {
      act(() => setup.renderer.destroy());
    }
  }
});
