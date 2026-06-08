import { config } from "../../core/config.ts";
import {
  applyStack,
  rebaseStack,
  stackStatus,
  type StackStatusReport,
} from "../../core/stack-ops.ts";
import { listStackManifests } from "../../core/wtstate.ts";
import { bold, cyan, dim, green, red, yellow } from "../colors.ts";

const HELP = `usage: wt stack <subcommand> [options]

subcommands:
  apply <stackId>            materialize a planned manifest into worktrees + draft PRs
  status [stackId]           render the manifest DAG + drift vs reality
  rebase <stackId>           regenerate engine links, sync/land, reconcile manifest

apply options:
  --install                  run install per slice (default off — slow)
status options:
  --json                     machine-readable output
rebase options:
  --onto <ref>               trunk to reparent landed slices onto (default ${config.branch.base})
  --merge                    land via merge queue (stack merge --auto) instead of sync`;

function logLine(line: string): void {
  console.log(dim(line));
}

async function runApply(argv: string[]): Promise<number> {
  let stackId: string | undefined;
  let install = false;
  for (const a of argv) {
    if (a === "--install") install = true;
    else if (a.startsWith("--")) {
      console.error(red(`unknown flag: ${a}`));
      return 2;
    } else if (!stackId) stackId = a;
    else {
      console.error(red(`unexpected arg: ${a}`));
      return 2;
    }
  }
  if (!stackId) {
    console.error(red("usage: wt stack apply <stackId> [--install]"));
    return 2;
  }
  const result = await applyStack(stackId, { install }, logLine);
  if (result.error) {
    console.error(red(result.error));
    if (result.materialized.length > 0) {
      console.error(
        dim(`(materialized before failure: ${result.materialized.join(", ")})`),
      );
    }
    return 1;
  }
  console.log(
    green(
      `✓ applied ${bold(stackId)} · ${result.materialized.length} materialized, ${result.skipped.length} skipped`,
    ),
  );
  return 0;
}

function renderStatus(report: StackStatusReport): void {
  const { manifest, rows } = report;
  console.log(`${bold("stack")} ${cyan(manifest.stackId)} ${dim(`· ${manifest.issue}`)}`);
  const tag = manifest.archivedTag ? dim(` [${manifest.archivedTag}]`) : dim(" (not yet tagged)");
  console.log(`  ${dim("origin")} ${manifest.holisticBranch}${tag}`);
  for (const { slice, expectedBase, live, drift } of rows) {
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
    console.log(
      `  ${ord} ${bold(slice.title)}${over}  ${pr} ${state}  ${dim("base:")} ${baseStr}${driftMark}`,
    );
  }
}

async function runStatus(argv: string[]): Promise<number> {
  let stackId: string | undefined;
  let json = false;
  for (const a of argv) {
    if (a === "--json") json = true;
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

  const ids = stackId ? [stackId] : manifests.map((m) => m.stackId);
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

async function runRebase(argv: string[]): Promise<number> {
  let stackId: string | undefined;
  let onto: string | undefined;
  let merge = false;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]!;
    if (a === "--merge") merge = true;
    else if (a === "--onto") {
      onto = argv[++i];
      if (!onto) {
        console.error(red("--onto requires a ref"));
        return 2;
      }
    } else if (a.startsWith("--")) {
      console.error(red(`unknown flag: ${a}`));
      return 2;
    } else if (!stackId) stackId = a;
    else {
      console.error(red(`unexpected arg: ${a}`));
      return 2;
    }
  }
  if (!stackId) {
    console.error(red("usage: wt stack rebase <stackId> [--onto <ref>] [--merge]"));
    return 2;
  }
  const result = await rebaseStack(
    stackId,
    { merge, ...(onto ? { onto } : {}) },
    logLine,
  );
  if (!result.ok) {
    console.error(red(result.error));
    if (result.conflict) {
      // Hand off to the resolving skill — wt never auto-resolves conflicts.
      if (result.failedBranch) console.error(yellow(`  failing branch: ${result.failedBranch}`));
      if (result.backupBranch) console.error(yellow(`  backup branch:  ${result.backupBranch}`));
      console.error(dim("  resolve the conflict, then re-run (or use /restack)."));
      return 3;
    }
    return 1;
  }
  if (result.output) console.log(dim(result.output));
  console.log(green(`✓ rebased ${bold(stackId)}`));
  return 0;
}

export async function run(argv: string[]): Promise<number> {
  const [sub, ...rest] = argv;
  if (!sub || sub === "--help" || sub === "-h") {
    console.log(HELP);
    return sub ? 0 : 2;
  }
  switch (sub) {
    case "apply":
      return runApply(rest);
    case "status":
      return runStatus(rest);
    case "rebase":
      return runRebase(rest);
    default:
      console.error(red(`unknown stack subcommand: ${sub}\n`));
      console.error(HELP);
      return 2;
  }
}
