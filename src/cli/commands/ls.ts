import { fetchPrs } from "../../core/github.ts";
import { linearUrlForSlug } from "../../core/linear.ts";
import type { Worktree } from "../../core/types.ts";
import { StatusKind } from "../../core/types.ts";
import {
  fetchOrigin,
  listWorktrees,
  unpushedCommits,
  worktreeStatus,
} from "../../core/worktree.ts";
import { dim } from "../colors.ts";
import {
  renderPrCell,
  renderSlugCell,
  renderStageCell,
  renderStatusCell,
  renderTable,
} from "../render.ts";
import { existsSync } from "node:fs";

export async function run(argv: string[]): Promise<number> {
  const jsonOut = argv.includes("--json");
  const all = await listWorktrees();
  const rows = all.filter((w) => !w.isMain);

  if (jsonOut) {
    const payload = await Promise.all(
      rows.map(async (w) => {
        const st = await worktreeStatus(w);
        const dirty = st.kind === StatusKind.Dirty;
        return {
          slug: w.slug,
          branch: w.branch,
          path: w.path,
          stage: w.stage,
          exists: existsSync(w.path),
          status: st.kind,
          status_label: st.label,
          status_age: st.age ?? null,
          dirty,
          unpushed: dirty ? 0 : await unpushedCommits(w.path),
          linear_url: linearUrlForSlug(w.slug),
        };
      }),
    );
    console.log(JSON.stringify(payload, null, 2));
    return 0;
  }

  if (rows.length === 0) {
    console.log(dim("No worktrees."));
    return 0;
  }

  // Parallel: PR fetch, origin fetch, status checks. Status needs fresh
  // refs, so await fetch first.
  const [prs] = await Promise.all([fetchPrs(), fetchOrigin()]);
  const statuses = await Promise.all(rows.map((w) => worktreeStatus(w)));

  type Row = { wt: Worktree; idx: number };
  const tableRows: Row[] = rows.map((wt, idx) => ({ wt, idx }));
  const table = renderTable(tableRows, [
    { header: "slug", getter: (r) => renderSlugCell((r as Row).wt) },
    { header: "stage", getter: (r) => renderStageCell((r as Row).wt) },
    { header: "pr", getter: (r) => renderPrCell((r as Row).wt, prs) },
    { header: "", getter: (r) => renderStatusCell(statuses[(r as Row).idx]!) },
  ]);
  console.log(table);
  return 0;
}
