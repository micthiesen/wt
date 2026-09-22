import { config } from "../../core/config.ts";
import { resolveIssueId } from "../../core/issue-tracker.ts";
import { issueStatusBadge } from "../badges.ts";
import { theme } from "../theme.ts";
import type { RowModule } from "./types.ts";

/** Shared by local and remote details. Open/copy still uses the full issue URL. */
export function IssueLine({ id, githubIssue, status, optimistic = false }: {
  id: string | null; githubIssue?: number | null; status?: string; optimistic?: boolean;
}) {
  if (!id && !githubIssue) return <text fg={theme.fgDim}>—</text>;
  const badge = issueStatusBadge(status);
  return (
    <text wrapMode="none" truncate>
      {id ? <span fg={badge.fg}>{badge.glyph}  #{id}</span> : null}
      {id && githubIssue ? <span fg={theme.fgDim}> ← </span> : null}
      {githubIssue ? <span fg={theme.fg}>{`#${githubIssue}`}</span> : null}
      {id && status ? <><span fg={theme.fgDim}> · </span><span fg={badge.fg}>{status}</span></> : null}
      {id && status && optimistic ? <span fg={theme.fgDim}> (updating)</span> : null}
    </text>
  );
}

export const issueRow: RowModule = {
  id: "issue",
  label: "issue",
  // No [issue_tracker] section = no issue-tracker concept. Hide the row
  // rather than render a permanent "—". The section alone (no
  // url_template) shows the bare parsed id; a template links it.
  visible: () => config.issueTracker !== null,
  sources: ({ row }) => row.issueStatus ? [row.issueStatus] : [],
  render: ({ row }) => IssueLine({
    id: resolveIssueId(row.wt.slug, row.issueId), githubIssue: row.githubIssue,
    status: row.issueStatus?.data, optimistic: row.issueStatus?.optimistic,
  }),
};
