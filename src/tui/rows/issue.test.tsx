import { expect, test } from "bun:test";
import type { ReactNode } from "react";
import type { RowContext } from "./types.ts";
import { IssueLine, issueRow } from "./issue.tsx";
import { issueStatusBadge } from "../badges.ts";
import { NF } from "../icons.ts";
import { theme } from "../theme.ts";

function content(node: ReactNode): string {
  if (typeof node === "string" || typeof node === "number") return String(node);
  if (Array.isArray(node)) return node.map(content).join("");
  if (node && typeof node === "object" && "props" in node) return content((node.props as { children?: ReactNode }).children);
  return "";
}

test("selected issue row renders tracker status independently from wt work status", () => {
  const row = {
    wt: { slug: "eng-1-example" }, issueId: "ENG-2", githubIssue: null,
    issueStatus: { data: "Awaiting customer", optimistic: false, isFetching: false, error: null },
    work: { state: "ready" },
  } as unknown as RowContext["row"];
  const ctx = { row } as RowContext;
  expect(content(issueRow.render(ctx))).toContain("Awaiting customer");
  expect(content(issueRow.render(ctx))).not.toContain("ready");
  expect(issueRow.sources?.(ctx)).toEqual([row.issueStatus!]);
  row.issueStatus!.optimistic = true;
  expect(content(issueRow.render(ctx))).toContain("Awaiting customer (updating)");
  row.issueStatus = undefined;
  expect(content(issueRow.render(ctx))).not.toContain("Awaiting customer");
  expect(issueRow.sources?.(ctx)).toEqual([]);
});

test("compact tracker and GitHub identity preserve their relationship without a second status", () => {
  expect(content(IssueLine({ id: "ENG-123", githubIssue: 456, status: "In Review" }))).toContain("#ENG-123 ← #456 · In Review");
  expect(content(IssueLine({ id: null, githubIssue: 456, status: "Stale", optimistic: true }))).toBe("#456");
  expect(content(IssueLine({ id: "ENG-123" }))).toContain("#ENG-123");
  expect(content(IssueLine({ id: null }))).toBe("—");
  expect(content(IssueLine({ id: "ENG-123", status: "In Review" }))).not.toContain("http");
});

test("tracker badges use configured frontend colors and neutral unknown status", () => {
  const styles = { "QA": { icon: "review" as const, color: "#2563EB" }, "Done": { icon: "completed" as const, color: "#047857" } };
  expect(issueStatusBadge("QA", styles)).toEqual({ glyph: NF.halfCircle, fg: "#2563EB" });
  expect(issueStatusBadge("Done", styles)).toEqual({ glyph: NF.taskComplete, fg: "#047857" });
  expect(issueStatusBadge("Unknown", styles)).toEqual({ glyph: NF.dotOutline, fg: theme.fgDim });
  expect(issueStatusBadge(undefined, styles)).toEqual({ glyph: NF.dotOutline, fg: theme.fgDim });
  expect(issueStatusBadge("toString", styles)).toEqual({ glyph: NF.dotOutline, fg: theme.fgDim });
});
