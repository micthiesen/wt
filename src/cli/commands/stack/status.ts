import { config } from "../../../core/config.ts";
import { STACK_CONNECTOR, layoutStack } from "../../../core/stack-layout.ts";
import type { SliceStatusRow, StackStatusReport } from "../../../core/stack-ops.ts";
import { stackStatus } from "../../../core/stack-ops.ts";
import { listStackManifests } from "../../../core/wtstate.ts";
import { bold, cyan, dim, green, red, yellow } from "../../colors.ts";
import { laneColor, stackIdFromCwd } from "./shared.ts";

function renderStatus(report: StackStatusReport): void {
  const { manifest, rows } = report;
  console.log(`${bold("stack")} ${cyan(manifest.stackId)} ${dim(`· ${manifest.issue}`)}`);
  const tag = manifest.archivedTag ? dim(` [${manifest.archivedTag}]`) : dim(" (not yet tagged)");
  console.log(`  ${dim("origin")} ${manifest.holisticBranch}${tag}`);

  const printRow = (r: SliceStatusRow, connector: string | null): void => {
    const { slice, expectedBase, live, drift } = r;
    const ord = cyan(String(slice.ordinal).padStart(2, "0"));
    const pr = slice.pr ? cyan(`#${slice.pr}`) : dim("(no pr)");
    const state =
      slice.status === "merged"
        ? green("merged")
        : slice.status === "open"
          ? (live?.state === "MERGED" ? green("merged") : yellow("open"))
          : dim("planned");
    const baseStr = expectedBase === config.branch.base ? dim(expectedBase) : expectedBase;
    const driftMark = drift ? red(`  ✗ ${drift}`) : green("  ✓");
    const over = slice.oversized ? dim(" (oversized)") : "";
    const spine = connector ? `${connector} ` : "";
    console.log(
      `  ${spine}${ord} ${bold(slice.title)}${over}  ${pr} ${state}  ${dim("base:")} ${baseStr}${driftMark}`,
    );
  };

  // Render the manifest DAG as a tree: `layoutStack` orders rows by lane
  // (a fork's branches stay contiguous) and hands each one a connector
  // glyph + lane index, identical to the TUI gutter. A malformed manifest
  // (cycle / dangling parent) drops the affected slices from `nodes`, so
  // fall back to the flat ordinal list rather than hiding them.
  const layout = layoutStack(manifest);
  const rowById = new Map(rows.map((r) => [r.slice.id, r]));
  if (layout.nodes.length === rows.length) {
    for (const n of layout.nodes) {
      const r = rowById.get(n.slice.id);
      if (r) printRow(r, laneColor(n.lane, STACK_CONNECTOR[n.pos]));
    }
  } else {
    for (const r of rows) printRow(r, null);
  }
}

export async function runStatus(argv: string[]): Promise<number> {
  let stackId: string | undefined;
  let json = false;
  let all = false;
  for (const a of argv) {
    if (a === "--json") json = true;
    else if (a === "--all") all = true;
    else if (a.startsWith("--")) {
      console.error(red(`unknown flag: ${a}`));
      return 2;
    } else if (!stackId) stackId = a;
    else {
      console.error(red(`unexpected arg: ${a}`));
      return 2;
    }
  }

  const manifests = listStackManifests();
  if (manifests.length === 0) {
    if (json) console.log("[]");
    else console.log(dim("No stack manifests."));
    return 0;
  }

  // Default to the current branch's stack (mirrors rebase/replay/reconcile);
  // `--all` or running from outside any stack falls back to every manifest.
  // Scoping by default keeps cross-stack slice references ("slice 04") from
  // colliding — every stack has a slice 04.
  const resolvedId = stackId ?? (all ? undefined : ((await stackIdFromCwd()) ?? undefined));
  const ids = resolvedId ? [resolvedId] : manifests.map((m) => m.stackId);
  const reports: StackStatusReport[] = [];
  for (const id of ids) {
    const r = await stackStatus(id);
    if (!r) {
      console.error(red(`no stack manifest: ${id}`));
      return 1;
    }
    reports.push(r);
  }

  if (json) {
    console.log(
      JSON.stringify(
        reports.map((r) => ({
          stackId: r.manifest.stackId,
          issue: r.manifest.issue,
          holisticBranch: r.manifest.holisticBranch,
          archivedTag: r.manifest.archivedTag ?? null,
          limits: r.manifest.limits,
          slices: r.rows.map(({ slice, expectedBase, live, drift }) => ({
            id: slice.id,
            ordinal: slice.ordinal,
            title: slice.title,
            branch: slice.branch,
            base: slice.base,
            expectedBase,
            dependsOn: slice.dependsOn,
            pr: slice.pr,
            status: slice.status,
            oversized: slice.oversized,
            liveBase: live?.baseRefName ?? null,
            liveState: live?.state ?? null,
            drift,
          })),
        })),
        null,
        2,
      ),
    );
    return 0;
  }

  reports.forEach((r, i) => {
    if (i > 0) console.log();
    renderStatus(r);
  });
  return 0;
}
