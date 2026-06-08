import { readFileSync, statSync } from "node:fs";

import { config } from "../../core/config.ts";
import {
  applyStack,
  rebaseStack,
  reconcileStack,
  replayStack,
  stackStatus,
  type RebaseResult,
  type StackStatusReport,
} from "../../core/stack-ops.ts";
import { git } from "../../core/git.ts";
import {
  findStackIdByBranch,
  getStackManifest,
  listStackManifests,
  putStackManifest,
  validateStackManifest,
} from "../../core/wtstate.ts";
import { bold, cyan, dim, green, red, yellow } from "../colors.ts";

const HELP = `usage: wt stack <subcommand> [options]

subcommands:
  apply <stackId>            materialize an already-ingested manifest
  apply --from <file>        strict-validate + ingest a manifest, then materialize
  plan --from <file>         strict-validate + ingest only (no materialize); prints stackId
  status [stackId]           render the manifest DAG + drift vs reality
  reconcile [stackId]        manifest bookkeeping only: mark merged PRs, reparent children
  replay [stackId]           squash-safe replay each slice onto its parent (+ retarget PRs)
  rebase [stackId]           reconcile then replay (the one-shot /restack does)
                             (stackId defaults to the current branch's stack)

apply options:
  --from <file>              ingest a skill-authored manifest JSON (strict validation)
  --install                  run install per slice (default off — slices are install-free)
status options:
  --json                     machine-readable output
reconcile/replay/rebase options:
  --onto <ref>               trunk landed roots reparent onto (default ${config.branch.base})`;

function logLine(line: string): void {
  console.log(dim(line));
}

/**
 * Resolve a stackId from the current worktree's branch, for subcommands
 * run from inside a slice without an explicit id. Returns null on a
 * detached HEAD or a branch that belongs to no manifest.
 */
async function stackIdFromCwd(): Promise<string | null> {
  let branch = "";
  try {
    branch = (await git(["rev-parse", "--abbrev-ref", "HEAD"], process.cwd())).trim();
  } catch {
    return null;
  }
  if (!branch || branch === "HEAD") return null;
  return findStackIdByBranch(branch);
}

type IngestResult =
  | { ok: true; stackId: string; sliceCount: number }
  | { ok: false };

/**
 * Read + STRICT-validate a skill-authored manifest file, then store it
 * via `putStackManifest`. This is the ONLY boundary by which a manifest
 * enters wt state — skills never write `state.json`. Validation errors
 * print verbatim (all of them) and the manifest is NOT stored.
 */
function ingestManifest(file: string): IngestResult {
  let text: string;
  try {
    if (!statSync(file).isFile()) {
      console.error(red(`not a file: ${file}`));
      return { ok: false };
    }
    text = readFileSync(file, "utf8");
  } catch (e) {
    console.error(red(`cannot read ${file}: ${e instanceof Error ? e.message : String(e)}`));
    return { ok: false };
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch (e) {
    console.error(red(`invalid JSON in ${file}: ${e instanceof Error ? e.message : String(e)}`));
    return { ok: false };
  }
  const v = validateStackManifest(parsed);
  if (!v.ok) {
    console.error(
      red(`manifest validation failed (${v.errors.length} error${v.errors.length === 1 ? "" : "s"}):`),
    );
    for (const err of v.errors) console.error(red(`  • ${err}`));
    return { ok: false };
  }
  // Refuse to clobber an already-materialized stack: a wholesale replace
  // would drop the `pr`/`status` mutations apply recorded. Re-materialize
  // via `wt stack apply <stackId>` instead (it's idempotent).
  const existing = getStackManifest(v.manifest.stackId);
  if (existing && existing.slices.some((s) => s.status !== "planned")) {
    console.error(
      red(
        `stack ${v.manifest.stackId} is already materialized (has open/merged slices) — ` +
          `re-ingesting would discard recorded PRs. Run \`wt stack apply ${v.manifest.stackId}\` instead.`,
      ),
    );
    return { ok: false };
  }
  try {
    putStackManifest(v.manifest);
  } catch (e) {
    console.error(red(`cannot store manifest: ${e instanceof Error ? e.message : String(e)}`));
    return { ok: false };
  }
  return { ok: true, stackId: v.manifest.stackId, sliceCount: v.manifest.slices.length };
}

async function runApply(argv: string[]): Promise<number> {
  let stackId: string | undefined;
  let install = false;
  let from: string | undefined;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]!;
    if (a === "--install") install = true;
    else if (a === "--from") {
      from = argv[++i];
      if (!from) {
        console.error(red("--from requires a path"));
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
  if (from) {
    if (stackId) {
      console.error(red("pass either --from <file> or <stackId>, not both"));
      return 2;
    }
    const ingested = ingestManifest(from);
    if (!ingested.ok) return 1;
    stackId = ingested.stackId;
    console.log(
      green(`✓ ingested ${bold(stackId)} (${ingested.sliceCount} slices) → materializing`),
    );
  }
  if (!stackId) {
    console.error(red("usage: wt stack apply <stackId> | --from <manifest.json> [--install]"));
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

async function runPlan(argv: string[]): Promise<number> {
  let from: string | undefined;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]!;
    if (a === "--from") {
      from = argv[++i];
      if (!from) {
        console.error(red("--from requires a path"));
        return 2;
      }
    } else {
      console.error(red(`unexpected arg: ${a}`));
      return 2;
    }
  }
  if (!from) {
    console.error(red("usage: wt stack plan --from <manifest.json>"));
    return 2;
  }
  const ingested = ingestManifest(from);
  if (!ingested.ok) return 1;
  console.log(
    green(
      `✓ ingested ${bold(ingested.stackId)} (${ingested.sliceCount} slices) — run \`wt stack apply ${ingested.stackId}\` to materialize`,
    ),
  );
  return 0;
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

/**
 * Parse the shared `[stackId] [--onto <ref>]` form for rebase/replay/
 * reconcile, resolving the id from the current branch when omitted. Returns
 * the parsed target, or a numeric exit code on a usage error.
 */
async function parseStackTarget(
  argv: string[],
  verb: string,
): Promise<{ stackId: string; onto?: string } | number> {
  let stackId: string | undefined;
  let onto: string | undefined;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]!;
    if (a === "--onto") {
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
    // No explicit id: resolve from the current worktree's branch so
    // `/restack` (and a human in a slice) can just run the command bare.
    stackId = (await stackIdFromCwd()) ?? undefined;
    if (stackId) console.log(dim(`stack ${stackId} (resolved from current branch)`));
  }
  if (!stackId) {
    console.error(
      red(
        `usage: wt stack ${verb} [<stackId>] [--onto <ref>]\n` +
          "  (no stackId given and the current branch isn't a tracked slice)",
      ),
    );
    return 2;
  }
  return onto ? { stackId, onto } : { stackId };
}

/** Render a rebase/replay result; returns the process exit code. */
function reportReplayResult(
  stackId: string,
  verb: string,
  result: RebaseResult,
): number {
  if (!result.ok) {
    console.error(red(result.error));
    if (result.conflict) {
      // Hand off to the resolving skill — wt never auto-resolves conflicts.
      if (result.failedBranch) console.error(yellow(`  failing branch: ${result.failedBranch}`));
      if (result.backupBranch) console.error(yellow(`  backup branch:  ${result.backupBranch}`));
      console.error(dim("  resolve in that worktree, then re-run `wt stack replay` (or /restack)."));
      return 3;
    }
    return 1;
  }
  if (result.output) console.log(dim(result.output));
  console.log(green(`✓ ${verb} ${bold(stackId)}`));
  return 0;
}

async function runRebase(argv: string[]): Promise<number> {
  const t = await parseStackTarget(argv, "rebase");
  if (typeof t === "number") return t;
  const opts = t.onto ? { onto: t.onto } : {};
  return reportReplayResult(t.stackId, "rebased", await rebaseStack(t.stackId, opts, logLine));
}

async function runReplay(argv: string[]): Promise<number> {
  const t = await parseStackTarget(argv, "replay");
  if (typeof t === "number") return t;
  const opts = t.onto ? { onto: t.onto } : {};
  return reportReplayResult(t.stackId, "replayed", await replayStack(t.stackId, opts, logLine));
}

async function runReconcile(argv: string[]): Promise<number> {
  const t = await parseStackTarget(argv, "reconcile");
  if (typeof t === "number") return t;
  if (!getStackManifest(t.stackId)) {
    console.error(red(`no stack manifest: ${t.stackId}`));
    return 1;
  }
  await reconcileStack(t.stackId, t.onto ?? config.branch.base, logLine);
  console.log(green(`✓ reconciled ${bold(t.stackId)}`));
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
    case "plan":
      return runPlan(rest);
    case "status":
      return runStatus(rest);
    case "reconcile":
      return runReconcile(rest);
    case "replay":
      return runReplay(rest);
    case "rebase":
      return runRebase(rest);
    default:
      console.error(red(`unknown stack subcommand: ${sub}\n`));
      console.error(HELP);
      return 2;
  }
}
