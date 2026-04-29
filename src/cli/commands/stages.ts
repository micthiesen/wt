import { config } from "../../core/config.ts";
import { categorizeStages, humanSize, listSstStages } from "../../core/sst.ts";
import type { SstStage } from "../../core/types.ts";
import { listWorktrees } from "../../core/worktree.ts";
import { cyan, dim, green, red, yellow } from "../colors.ts";
import { humanAge } from "../../core/locks.ts";
import { renderTable } from "../render.ts";
import { confirm, isInteractive } from "../prompt.ts";

function parseFlags(argv: string[]): { json: boolean; clean: boolean; yes: boolean } {
  return {
    json: argv.includes("--json"),
    clean: argv.includes("--clean"),
    yes: argv.includes("--yes") || argv.includes("-y"),
  };
}

function ageOf(s: SstStage, now: number): string {
  const t = Date.parse(s.lastModified);
  if (Number.isNaN(t)) return "?";
  return humanAge((now - t) / 1000);
}

export async function run(argv: string[]): Promise<number> {
  const { json, clean, yes } = parseFlags(argv);

  if (!config.sst) {
    console.error(red("[deploy.sst] is not configured in config.toml; nothing to do."));
    return 2;
  }

  const stages = await listSstStages();
  if (!stages) {
    console.error(red("Failed to list SST state bucket."));
    return 1;
  }
  const wts = await listWorktrees();
  const worktreeStages = new Set(wts.filter((w) => !w.isMain).map((w) => w.stage));
  const { live, orphaned } = await categorizeStages(stages, worktreeStages);

  if (json) {
    console.log(
      JSON.stringify(
        {
          live: live.map((s) => ({ name: s.name, size_bytes: s.sizeBytes, modified: s.lastModified })),
          orphaned: orphaned.map((s) => ({
            name: s.name,
            size_bytes: s.sizeBytes,
            modified: s.lastModified,
          })),
        },
        null,
        2,
      ),
    );
  } else if (live.length === 0 && orphaned.length === 0) {
    console.log(dim(`No \`${config.stage.prefix}*\` stages in ${config.sst.stateBucket}.`));
  } else {
    const now = Date.now();
    type Row = { marker: string; stage: SstStage; status: string };
    const rows: Row[] = [
      ...orphaned.map((s) => ({ marker: yellow("⚠"), stage: s, status: yellow("orphaned") })),
      ...live.map((s) => ({ marker: green("✓"), stage: s, status: green("live") })),
    ];
    const table = renderTable(rows, [
      { header: "", getter: (r) => (r as Row).marker },
      { header: "stage", getter: (r) => cyan((r as Row).stage.name) },
      { header: "size", getter: (r) => dim(humanSize((r as Row).stage.sizeBytes)) },
      { header: "age", getter: (r) => dim(ageOf((r as Row).stage, now)) },
      { header: "status", getter: (r) => (r as Row).status },
    ]);
    console.log(table);
    console.log(
      dim(`  ${live.length} live · ${orphaned.length} orphaned · bucket ${config.sst.stateBucket}`),
    );
  }

  if (!clean) return 0;
  if (orphaned.length === 0) {
    if (!json) console.log(green("No orphans to clean."));
    return 0;
  }
  if (!yes) {
    if (!isInteractive()) {
      console.error(red("Use -y with --clean in non-interactive mode."));
      return 2;
    }
    if (!(await confirm(`Destroy ${orphaned.length} orphaned stage(s)?`, false))) {
      return 0;
    }
  }
  for (const s of orphaned) {
    console.log();
    console.log(`--- Destroying ${red(s.name)} ---`);
    const p = Bun.spawn(["pnpm", "sst", "remove", "--stage", s.name], {
      cwd: config.paths.mainClone,
      stdout: "inherit",
      stderr: "inherit",
    });
    const code = await p.exited;
    if (code === 0) console.log(green(`✓ destroyed ${s.name}`));
    else console.log(red(`✗ ${s.name} failed (exit ${code}); continuing`));
  }
  return 0;
}
