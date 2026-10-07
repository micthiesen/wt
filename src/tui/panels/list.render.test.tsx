import { expect, test } from "bun:test";
import { act } from "react";
import { useTerminalDimensions } from "@opentui/react";
import { testRender } from "@opentui/react/test-utils";

import { StatusKind } from "../../core/types.ts";
import type { FieldState, WorktreeRow } from "../hooks/useWorktreeRows.ts";
import { localWorktreeModel } from "../worktree-model.ts";
import { rowLabel, WorktreeList, type ListActiveItem } from "./list.tsx";

function ListFixture({ items = [] }: { items?: ListActiveItem[] }) {
  const { width } = useTerminalDimensions();
  return (
    <WorktreeList
      items={items}
      archivedItems={[]}
      reviewRequests={[]}
      selectedIndex={0}
      width={width}
      activeTails={new Set()}
      activeActions={new Set()}
      activeSessionBySlug={new Map()}
      isLoading={false}
      remoteUnavailable={false}
    />
  );
}

function field<T>(data?: T): FieldState<T> {
  return { data, isLoading: false, isFetching: false, isStale: false, error: null };
}

function row(slug: string, section: string): WorktreeRow {
  return {
    wt: { slug, branch: `ci/${slug}`, path: `/tmp/${slug}`, isMain: false, stage: slug },
    fields: {
      dirty: field(), lock: field(), deploy: field(), dev: field(),
      merged: field(), gone: field(), sync: field(), claude: field(),
      gitActivity: field(), conflict: field(),
    },
    status: { kind: StatusKind.Clean, label: "clean" },
    landedOn: null, work: null, githubIssue: null, issueId: null,
    archived: false, titleSource: "slug", section,
    title: "Long implementation name for a task with 日本語 and emoji 👩🏽‍💻 continued to overflow",
    stackedOn: null, stack: null,
  };
}

test("list uses the full canonical title with exact casing, ignoring any old brief", async () => {
  const item = { ...row("move-files-to-r2", ""), title: "iOS uploads move to R2 with previews intact", brief: "Old short label" };
  expect(rowLabel(item)).toBe(item.title);
  const model = localWorktreeModel(item);
  const items: ListActiveItem[] = [{ kind: "wt", row: item, model, target: model.target }];
  const setup = await testRender(<ListFixture items={items} />, { width: 80, height: 10 });
  try {
    await setup.flush();
    expect(setup.captureCharFrame()).toContain(item.title);
    act(() => setup.resize(30, 10));
    await setup.flush();
    const narrow = setup.captureCharFrame();
    expect(narrow).toContain("iOS uploads");
    expect(narrow).toContain("...");
    expect(narrow).not.toContain("Old short label");
  } finally { act(() => setup.renderer.destroy()); }
});

test("empty worktree hint wraps in reading order at narrow widths", async () => {
  const setup = await testRender(<ListFixture />, { width: 45, height: 16 });
  try {
    for (const width of [45, 30, 20]) {
      act(() => setup.resize(width, 16));
      await setup.flush();
      const prose = setup.captureCharFrame().split("\n")
        .filter((line) => line.startsWith("│"))
        .map((line) => line.slice(1, -1).trim()).filter(Boolean).join(" ");
      expect(prose).toContain("Press n to create one.");
    }
  } finally {
    act(() => setup.renderer.destroy());
  }
});

test("wide section names fit and split-parent references keep the truncation mark through resize", async () => {
  const section = "日本語の長いセクション名";
  const parent = row("parent", section);
  const child = row("child", "Other section");
  parent.stack = { stackId: parent.wt.branch, lane: 0, depth: 0, index: 0 };
  child.stack = { stackId: parent.wt.branch, lane: 0, depth: 1, index: 1 };
  child.stackedOn = { slug: parent.wt.slug, branch: parent.wt.branch, diffBase: parent.wt.branch };
  const items: ListActiveItem[] = [parent, child].map((row) => {
    const model = localWorktreeModel(row);
    return { kind: "wt", row, model, target: model.target };
  });
  const setup = await testRender(<ListFixture items={items} />, { width: 80, height: 16 });
  try {
    for (const width of [80, 30, 60, 80]) {
      act(() => setup.resize(width, 16));
      await setup.flush();
      const frame = setup.captureCharFrame();
      if (width >= 60) {
        expect(frame).toContain(`── ${section} `);
        expect(frame).toContain("... → ");
      } else {
        expect(frame).toContain("── 日本語の長いセクシ...");
        expect(frame).not.toContain(" → ");
      }
      expect(frame).not.toContain("�");
      expect(setup.renderer.root.findDescendantById("parent")?.height).toBe(1);
      expect(setup.renderer.root.findDescendantById("child")?.height).toBe(1);
    }
  } finally {
    act(() => setup.renderer.destroy());
  }
});
