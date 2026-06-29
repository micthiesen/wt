import { pickPrForWorktree, pullRequestOpenUrl } from "../core/github.ts";
import { linearUrlForSlug } from "../core/linear.ts";
import { isOurStageDeployed } from "../core/stage-safety.ts";
import { stageUrl } from "../core/stage.ts";
import type { PullRequest, Status, Worktree } from "../core/types.ts";
import { StatusKind } from "../core/types.ts";
import { cyan, dim, green, link, red, visibleWidth, yellow } from "./colors.ts";

export function renderSlugCell(wt: Worktree): string {
  const url = linearUrlForSlug(wt.slug);
  return url ? link(cyan(wt.slug), url) : cyan(wt.slug);
}

export function renderStageCell(wt: Worktree): string {
  if (isOurStageDeployed(wt)) {
    const url = stageUrl(wt.stage);
    return url ? link(wt.stage, url) : wt.stage;
  }
  return dim("(not deployed)");
}

export function renderPrCell(
  wt: Worktree,
  prs: Map<string, PullRequest>,
): string {
  const pr = pickPrForWorktree(wt, prs);
  if (!pr) return dim("—");
  const parts = [`#${pr.number}`];
  if (pr.state === "MERGED") parts.push(dim("(merged)"));
  else if (pr.state === "CLOSED") parts.push(dim("(closed)"));
  else if (pr.isDraft) parts.push(dim("(draft)"));
  return link(parts.join(" "), pullRequestOpenUrl(pr.url));
}

export function renderStatusCell(status: Status): string {
  switch (status.kind) {
    case StatusKind.Busy: {
      const base = status.age ? `${status.label} ${status.age}` : status.label;
      const text = status.log ? link(base, `file://${status.log}`) : base;
      return yellow(text);
    }
    case StatusKind.Missing:
      return red("missing");
    case StatusKind.Gone:
      return yellow("gone");
    case StatusKind.Merged:
      return green("merged");
    case StatusKind.Dirty:
      return yellow("●");
    case StatusKind.Clean:
    default:
      return "";
  }
}

type Col = { header: string; getter: (row: unknown) => string };

export function renderTable(
  rows: unknown[],
  columns: Col[],
  opts: { padding?: number; header?: boolean } = {},
): string {
  const { padding = 2, header = true } = opts;
  const data = rows.map((r) => columns.map((c) => c.getter(r)));
  const widths = columns.map((c, i) => {
    const colCells = [header ? c.header : "", ...data.map((r) => r[i] ?? "")];
    return Math.max(...colCells.map((s) => visibleWidth(s)));
  });
  const pad = " ".repeat(padding);
  const lines: string[] = [];
  if (header) {
    lines.push(
      columns.map((c, i) => padRight(c.header, widths[i]!)).join(pad),
    );
  }
  for (const row of data) {
    lines.push(row.map((c, i) => padRight(c, widths[i]!)).join(pad));
  }
  return lines.join("\n");
}

function padRight(s: string, width: number): string {
  const cur = visibleWidth(s);
  if (cur >= width) return s;
  return s + " ".repeat(width - cur);
}
