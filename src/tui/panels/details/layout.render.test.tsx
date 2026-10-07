import { expect, test } from "bun:test";
import { act, createRef } from "react";
import type { ScrollBoxRenderable } from "@opentui/core";
import { useTerminalDimensions } from "@opentui/react";
import { testRender } from "@opentui/react/test-utils";

import type { ReviewRequestPr } from "../../../core/github.ts";
import type { RemovedWorktree } from "../../../core/wtstate.ts";
import { RemovedBody } from "./removed-body.tsx";
import { ReviewRequestBody } from "./review-request-body.tsx";
import { SectionSummaryBody } from "./section-summary-body.tsx";
import { WorkStatusRecordBlock } from "./work-status-block.tsx";

const removed: RemovedWorktree = {
  slug: "test-long-status",
  branch: "feature/国际化-long-saved-branch",
  title: "Internationalized 国际化 history",
  removedAt: "2026-09-23T18:00:00Z",
  prNumber: 2150,
  prState: "MERGED",
  prUrl: "https://example.com/repo/pull/2150",
  work: {
    state: "ready",
    at: "2026-10-01T17:00:00Z",
    risk: "low",
    note: "Saved note with enough detail to require scrolling. ".repeat(15),
    verifyAfterMerge: "Check the deployment. STEPS: 1. Open the final URL. 2. Confirm the welcome text.",
  },
};

const review: ReviewRequestPr = {
  number: 44,
  url: "https://example.com/repo/pull/44",
  title: "Internationalized 国际化 review",
  repoNameWithOwner: "very-long-organization-name/equally-long-repository-name",
  headRefName: "feature/国际化-long-review-branch",
  author: "review-author",
  isDraft: false,
  checks: "fail",
  reviewDecision: "CHANGES_REQUESTED",
  additions: 44,
  deletions: 12,
  changedFiles: 6,
  commentCount: 3,
  createdAt: "2026-09-24T18:00:00Z",
  updatedAt: "2026-09-25T18:00:00Z",
};

test("long removed notes scroll without overprinting and retain restore hints", async () => {
  for (const width of [70, 40, 24]) {
    const scrollRef = createRef<ScrollBoxRenderable>();
    const setup = await testRender(
      <RemovedBody entry={removed} width={width} scrollRef={scrollRef} />,
      { width, height: 15 },
    );
    try {
      await setup.flush();
      const first = setup.captureCharFrame();
      const lines = first.split("\n");
      expect(lines[2]).toContain("Internat");
      const statusLine = lines.findIndex((line) => line.includes("● unver"));
      expect(statusLine).toBeGreaterThan(2);
      // Exclude pane padding and the scroll gutter as well as both borders.
      const title = lines.slice(2, statusLine).map((line) => line.slice(2, -3).trim()).join(" ");
      expect(title).toContain(removed.title!);
      expect(lines.some((line) => line.includes("│ Saved note"))).toBe(true);
      expect(lines[12]).toContain("⏎ restore");
      expect(lines[14]).toStartWith("└");
      expect(scrollRef.current!.scrollHeight).toBeGreaterThan(scrollRef.current!.viewport.height);
      act(() => scrollRef.current!.scrollBy(1, "content"));
      await setup.flush();
      const last = setup.captureCharFrame();
      expect(last).toContain("pr ");
      expect(last).toContain("removed");
      expect(last).toContain("2150");
      expect(last.split("\n")[12]).toBe(lines[12]);
      expect(last.split("\n")[14]).toStartWith("└");
    } finally {
      act(() => setup.renderer.destroy());
    }
  }
});

test("narrow status banners retain the complete work state and risk", async () => {
  const setup = await testRender(
    <box width={19} flexDirection="column">
      <WorkStatusRecordBlock
        record={{ state: "needs-human", risk: "medium", at: "2026-10-01T17:00:00Z" }}
        contentWidth={19}
        verifyExpanded={null}
        landed={false}
        lastCommitMs={null}
      />
    </box>,
    { width: 19, height: 8 },
  );
  try {
    await setup.flush();
    const frame = setup.captureCharFrame();
    expect(frame).toContain("needs-human");
    expect(frame).toContain("risk medium");
    expect(frame).not.toContain("...");
  } finally {
    act(() => setup.renderer.destroy());
  }
});

test("review identity survives narrow widths and review status wraps completely", async () => {
  for (const width of [70, 40, 24]) {
    const scrollRef = createRef<ScrollBoxRenderable>();
    const setup = await testRender(
      <ReviewRequestBody pr={review} width={width} scrollRef={scrollRef} />,
      { width, height: 20 },
    );
    try {
      await setup.flush();
      expect(setup.captureCharFrame().split("\n")[0]).toContain("very-long");
      act(() => scrollRef.current!.scrollBy(1, "content"));
      await setup.flush();
      const frame = setup.captureCharFrame();
      expect(frame).toContain("requested");
      expect(frame.split("\n")[19]).toStartWith("└");
    } finally {
      act(() => setup.renderer.destroy());
    }
  }
});

test("section details leave room for the adjacent activity pane through height resizes", async () => {
  const width = 40;
  function ResizingSection() {
    const { height } = useTerminalDimensions();
    return <box flexDirection="column" width={width} height={height}>
      <SectionSummaryBody
        section={{ sectionKey: "test", label: "Empty 国际化 section", members: [], pausedCount: 0 }}
        width={width}
        height={height - 5}
      />
      <box id="activity-fixture" height={5} flexShrink={0} border>
        <text>ACTIVITY PANEL</text>
      </box>
    </box>;
  }
  const setup = await testRender(
    <ResizingSection />,
    { width, height: 20 },
  );
  try {
    for (const height of [20, 15, 25]) {
      act(() => setup.resize(width, height));
      await setup.flush();
      const lines = setup.captureCharFrame().split("\n");
      expect(lines[height - 4]).toContain("ACTIVITY PANEL");
      expect(lines[height - 1]).toStartWith("└");
      expect(lines[height - 8]).toContain("TAB expand · y yank");
      expect(setup.renderer.root.findDescendantById("activity-fixture")!.height).toBe(5);
    }
  } finally {
    act(() => setup.renderer.destroy());
  }
});
