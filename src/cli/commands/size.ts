import { config } from "../../core/config.ts";
import { run as sh } from "../../core/proc.ts";
import { computeSize, type SizeReport } from "../../core/size.ts";
import {
  getStackManifest,
  listStackManifests,
  type StackManifest,
} from "../../core/wtstate.ts";
import { bold, cyan, dim, green, red, yellow } from "../colors.ts";

type Flags = {
  paths: string[];
  json: boolean;
  base: string;
  stackId?: string;
};

function parse(argv: string[]): Flags | { error: string } {
  const paths: string[] = [];
  let json = false;
  let base = `origin/${config.branch.base}`;
  let stackId: string | undefined;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]!;
    if (a === "--json") json = true;
    else if (a === "--base") {
      const v = argv[++i];
      if (!v) return { error: "--base requires a ref" };
      base = v;
    } else if (a === "--stack") {
      const v = argv[++i];
      if (!v) return { error: "--stack requires a stackId" };
      stackId = v;
    } else if (a === "--") {
      // Everything after `--` is a literal pathspec.
      paths.push(...argv.slice(i + 1));
      break;
    } else if (a.startsWith("--")) return { error: `unknown flag: ${a}` };
    else paths.push(a);
  }
  return { paths, json, base, ...(stackId ? { stackId } : {}) };
}

async function currentBranch(cwd: string): Promise<string> {
  const r = await sh(["git", "rev-parse", "--abbrev-ref", "HEAD"], { cwd });
  return r.exitCode === 0 ? r.stdout.trim() : "";
}

/** A manifest whose holistic branch is checked out here, if any. */
function manifestForBranch(branch: string): StackManifest | null {
  return (
    listStackManifests().find(
      (m) => m.holisticBranch === branch || m.slices.some((s) => s.branch === branch),
    ) ?? null
  );
}

function overBudget(report: SizeReport, m: StackManifest): boolean {
  return (
    report.prodLines > m.limits.prodLines || report.prodFiles > m.limits.files
  );
}

function budgetTag(report: SizeReport, m: StackManifest): string {
  if (!overBudget(report, m)) return green("within budget");
  const reason: string[] = [];
  if (report.prodLines > m.limits.prodLines) {
    reason.push(`${report.prodLines}>${m.limits.prodLines} lines`);
  }
  if (report.prodFiles > m.limits.files) {
    reason.push(`${report.prodFiles}>${m.limits.files} files`);
  }
  return yellow(`over budget (${reason.join(", ")})`);
}

function renderReport(branch: string, base: string, report: SizeReport): void {
  console.log(`${bold("size")} · ${cyan(branch || "(detached)")} ${dim(`vs ${base}`)}`);
  console.log(
    `  ${bold("prod")}   ${report.prodLines} lines  ${dim("across")} ${report.prodFiles} files`,
  );
  console.log(
    `  ${bold("total")}  ${report.totalLines} lines  ${dim("across")} ${report.files} files`,
  );
  const excluded = report.perFile.filter((f) => !f.production);
  if (excluded.length > 0) {
    console.log(
      dim(`  (excluded ${excluded.length} non-production file(s): tests, snapshots, generated, lockfiles)`),
    );
  }
}

/** Per-slice breakdown: size each slice's file group within the holistic diff. */
async function reportStack(
  m: StackManifest,
  base: string,
  jsonOut: boolean,
): Promise<void> {
  const cwd = config.paths.mainClone;
  const target = m.holisticBranch;
  const rows = await Promise.all(
    m.slices.map(async (s) => ({
      slice: s,
      report: await computeSize({ cwd, base, target, paths: s.files }),
    })),
  );
  if (jsonOut) {
    console.log(
      JSON.stringify(
        {
          stackId: m.stackId,
          base,
          holisticBranch: target,
          slices: rows.map(({ slice, report }) => ({
            id: slice.id,
            ordinal: slice.ordinal,
            title: slice.title,
            oversized: slice.oversized,
            ...report,
          })),
        },
        null,
        2,
      ),
    );
    return;
  }
  console.log(`${bold("stack")} ${cyan(m.stackId)} ${dim(`· slice sizes vs ${base}`)}`);
  for (const { slice, report } of rows) {
    const ord = String(slice.ordinal).padStart(2, "0");
    const tag = slice.oversized
      ? dim("(oversized, sanctioned)")
      : budgetTag(report, m);
    console.log(
      `  ${cyan(ord)} ${bold(slice.title)}  ${report.prodLines} lines / ${report.prodFiles} files  ${tag}`,
    );
  }
}

export async function run(argv: string[]): Promise<number> {
  const parsed = parse(argv);
  if ("error" in parsed) {
    console.error(red(parsed.error));
    return 2;
  }

  // Explicit per-slice breakdown.
  if (parsed.stackId) {
    const m = getStackManifest(parsed.stackId);
    if (!m) {
      console.error(red(`no stack manifest: ${parsed.stackId}`));
      return 1;
    }
    await reportStack(m, parsed.base, parsed.json);
    return 0;
  }

  const cwd = process.cwd();
  const branch = await currentBranch(cwd);

  // Fail loudly on an unresolvable base rather than silently reporting
  // zero (a diff against a bogus ref counts nothing — dangerous for a
  // budget read where "0 lines" reads as "trivially small").
  const baseOk = await sh(["git", "rev-parse", "--verify", "--quiet", parsed.base], { cwd });
  if (baseOk.exitCode !== 0) {
    console.error(red(`base ref does not resolve: ${parsed.base}`));
    return 1;
  }

  // If the checkout is a holistic branch with a manifest and no explicit
  // pathspecs were given, show the per-slice breakdown automatically.
  if (parsed.paths.length === 0) {
    const m = manifestForBranch(branch);
    if (m && m.holisticBranch === branch) {
      await reportStack(m, parsed.base, parsed.json);
      return 0;
    }
  }

  const report = await computeSize({
    cwd,
    base: parsed.base,
    ...(parsed.paths.length > 0 ? { paths: parsed.paths } : {}),
  });
  if (parsed.json) {
    console.log(JSON.stringify({ branch, base: parsed.base, ...report }, null, 2));
    return 0;
  }
  renderReport(branch, parsed.base, report);
  return 0;
}
