import {
  AWS_PROFILE,
  DEFAULT_PERSONAL_STAGE,
  SST_STATE_BUCKET,
  SST_STATE_PREFIX,
  STAGE_PREFIX,
} from "./paths.ts";
import { run } from "./proc.ts";
import type { SstStage } from "./types.ts";

export async function awsS3(args: string[]): Promise<{ stdout: string; ok: boolean }> {
  const r = await run(["aws", "s3", ...args, "--profile", AWS_PROFILE]);
  return { stdout: r.stdout, ok: r.exitCode === 0 };
}

/** List stages from the SST state bucket. Returns null on failure. */
export async function listSstStages(): Promise<SstStage[] | null> {
  const r = await awsS3(["ls", `s3://${SST_STATE_BUCKET}/${SST_STATE_PREFIX}`]);
  if (!r.ok) return null;
  const stages: SstStage[] = [];
  for (const line of r.stdout.split("\n")) {
    const parts = line.trim().split(/\s+/);
    if (parts.length < 4) continue;
    const date = parts[0]!;
    const time = parts[1]!;
    const sizeS = parts[2]!;
    const name = parts[3]!;
    if (!name.endsWith(".json")) continue;
    const size = parseInt(sizeS, 10);
    if (Number.isNaN(size)) continue;
    stages.push({
      name: name.slice(0, -".json".length),
      sizeBytes: size,
      lastModified: `${date}T${time}Z`,
    });
  }
  return stages;
}

/**
 * True if the stage's state file lists any resources. `sst remove`
 * leaves a small empty state file; without this check dead stages get
 * repeatedly flagged as orphans.
 */
async function stageHasResources(name: string): Promise<boolean> {
  const r = await awsS3([
    "cp",
    `s3://${SST_STATE_BUCKET}/${SST_STATE_PREFIX}${name}.json`,
    "-",
  ]);
  if (!r.ok) return true; // be conservative on read failure
  try {
    const state = JSON.parse(r.stdout);
    const resources =
      state?.checkpoint?.latest?.resources ?? [];
    return Array.isArray(resources) && resources.length > 0;
  } catch {
    return true;
  }
}

export async function categorizeStages(
  stages: SstStage[],
  worktreeStages: Set<string>,
): Promise<{ live: SstStage[]; orphaned: SstStage[] }> {
  const live: SstStage[] = [];
  const orphaned: SstStage[] = [];
  for (const s of stages) {
    if (s.name === DEFAULT_PERSONAL_STAGE) continue;
    if (!s.name.startsWith(STAGE_PREFIX)) continue;
    if (worktreeStages.has(s.name)) {
      live.push(s);
      continue;
    }
    if (!(await stageHasResources(s.name))) continue;
    orphaned.push(s);
  }
  orphaned.sort((a, b) => (b.lastModified > a.lastModified ? 1 : -1));
  live.sort((a, b) => a.name.localeCompare(b.name));
  return { live, orphaned };
}

export function humanSize(n: number): string {
  if (n < 1024) return `${n} B`;
  if (n < 1024 * 1024) return `${(n / 1024).toFixed(1)} KB`;
  if (n < 1024 * 1024 * 1024) return `${(n / (1024 * 1024)).toFixed(1)} MB`;
  return `${(n / (1024 * 1024 * 1024)).toFixed(1)} GB`;
}
