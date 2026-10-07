import { describe, expect, test } from "bun:test";

import { StatusKind } from "../../core/types.ts";
import { DEV_SERVER_STOPPED } from "../../core/dev-server.ts";
import type { WorkState } from "../../core/work-status.ts";
import {
  GROUP_INBOX,
  resolveTitle,
  sortActiveRows,
  type FieldState,
  type WorktreeRow,
} from "./useWorktreeRows.ts";

function field<T = never>(data?: T): FieldState<T> {
  return { data, isStale: false, isFetching: false, isLoading: false, error: null };
}

function row(slug: string, overrides: Partial<WorktreeRow> = {}): WorktreeRow {
  return {
    wt: { slug, branch: `test/${slug}`, path: `/unused/${slug}`, stage: slug, isMain: false },
    fields: {
      dirty: field([]), lock: field(null), deploy: field(false),
      dev: field(DEV_SERVER_STOPPED), merged: field(false), gone: field(false),
      sync: field(), claude: field(), gitActivity: field(),
      conflict: field(),
    },
    status: { kind: StatusKind.Clean, label: "clean" },
    landedOn: null, stackedOn: null, stack: null, githubIssue: null, issueId: null,
    work: null, archived: false, title: slug, titleSource: "llm", section: null,
    ...overrides,
  };
}

function work(state: WorkState): WorktreeRow["work"] {
  return { state, at: "2026-10-05T00:00:00Z" };
}

function slugs(rows: WorktreeRow[]): string[] {
  return rows.map((entry) => entry.wt.slug);
}

describe("resolveTitle", () => {
  test("a saved title wins over new AI, PR, and commit titles", () => {
    const first = resolveTitle("fix-launch", "Initial AI title", "PR title", "Commit title");
    expect(first).toEqual({ title: "Initial AI title", source: "llm" });

    // Accepting the prefilled title is still a manual choice. A late AI result
    // and subsequent PR/commit changes must not replace the accepted text.
    expect(resolveTitle("fix-launch", "Replacement AI title", "Renamed PR", "New commit", first.title))
      .toEqual({ title: "Initial AI title", source: "manual" });
  });

  test("untitled worktrees retain the existing fallback chain", () => {
    expect(resolveTitle("fix-launch", "AI title", "PR title", "Commit title"))
      .toEqual({ title: "AI title", source: "llm" });
    expect(resolveTitle("fix-launch", null, "PR title", "Commit title"))
      .toEqual({ title: "PR title", source: "pr" });
    expect(resolveTitle("fix-launch", null, null, "Commit title"))
      .toEqual({ title: "Commit title", source: "commit" });
    expect(resolveTitle("fix-launch", null, null, null))
      .toEqual({ title: "Fix launch", source: "slug" });
  });
});

describe("sortActiveRows", () => {
  test("tied rows keep slug order when AI titles and inventory enumeration change", () => {
    const before = [row("zebra", { title: "A title" }), row("alpha", { title: "Z title" })];
    const after = [row("alpha", { title: "A new title" }), row("zebra", { title: "Z new title" })];
    for (const statusSort of [true, false]) {
      expect(slugs(sortActiveRows(before, new Map(), [GROUP_INBOX], statusSort)))
        .toEqual(["alpha", "zebra"]);
      expect(slugs(sortActiveRows(after, new Map(), [GROUP_INBOX], statusSort)))
        .toEqual(["alpha", "zebra"]);
    }
    expect(slugs(before)).toEqual(["zebra", "alpha"]);
  });

  test("status remains primary and manual order remains the first tie-break", () => {
    const rows = [
      row("alpha-working", { work: work("working") }),
      row("bravo-ready", { work: work("ready") }),
      row("zebra-ready", { work: work("ready") }),
    ];
    const orders = new Map([["alpha-working", 0], ["bravo-ready", 2], ["zebra-ready", 1]]);
    expect(slugs(sortActiveRows(rows, orders, [GROUP_INBOX], true)))
      .toEqual(["zebra-ready", "bravo-ready", "alpha-working"]);
    expect(slugs(sortActiveRows(rows, orders, [GROUP_INBOX], false)))
      .toEqual(["alpha-working", "zebra-ready", "bravo-ready"]);
  });

  test("an unranked new row precedes manually ranked peers in its status band", () => {
    const rows = [row("alpha"), row("zebra")];
    expect(slugs(sortActiveRows(rows, new Map([["alpha", 0]]), [GROUP_INBOX], true)))
      .toEqual(["zebra", "alpha"]);
  });

  test("section order still takes precedence over status and slug", () => {
    const rows = [
      row("alpha", { section: "Later", work: work("ready") }),
      row("zebra", { section: "First", work: work("working") }),
      row("middle"),
    ];
    expect(slugs(sortActiveRows(rows, new Map(), ["First", GROUP_INBOX, "Later"], true)))
      .toEqual(["zebra", "middle", "alpha"]);
  });

  test("equal-ranked stack members stay contiguous in spine order across inventory changes", () => {
    const parent = row("a-parent", {
      stack: { stackId: "b-stack", index: 0, depth: 0, lane: 0 }, title: "Z title",
    });
    const child = row("z-child", {
      stack: { stackId: "b-stack", index: 1, depth: 1, lane: 0 }, title: "A title",
    });
    const unrelated = row("c-unrelated");
    for (const rows of [[child, unrelated, parent], [unrelated, parent, child]]) {
      expect(slugs(sortActiveRows(rows, new Map(), [GROUP_INBOX], true)))
        .toEqual(["a-parent", "z-child", "c-unrelated"]);
    }
  });

  test("a stack uses its most urgent status and its root's manual position", () => {
    const parent = row("parent", {
      stack: { stackId: "stack", index: 0, depth: 0, lane: 0 }, work: work("working"),
    });
    const child = row("child", {
      stack: { stackId: "stack", index: 1, depth: 1, lane: 0 }, work: work("ready"),
    });
    const peer = row("peer", { work: work("ready") });
    expect(slugs(sortActiveRows([child, peer, parent],
      new Map([["parent", 0], ["peer", 1], ["child", 100]]), [GROUP_INBOX], true)))
      .toEqual(["parent", "child", "peer"]);
    expect(slugs(sortActiveRows([child, peer, parent],
      new Map([["parent", 2], ["peer", 1], ["child", 0]]), [GROUP_INBOX], true)))
      .toEqual(["peer", "parent", "child"]);
  });
});
