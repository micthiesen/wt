import { readFileSync, statSync } from "node:fs";
import { basename } from "node:path";

import { config } from "../../core/config.ts";
import {
  addSliceToStack,
  applyStack,
  pruneStackBackups,
  rebaseStack,
  reconcileStack,
  replayStack,
  splitStack,
  stackStatus,
  validateFileCoverage,
  type RebaseResult,
  type SliceStatusRow,
  type StackStatusReport,
  type SubSliceSpec,
} from "../../core/stack-ops.ts";
import { git, gitRun } from "../../core/git.ts";
import { DEFAULT_HUNK_CONTEXT, fileHunks, holisticBase, hunkLineCounts } from "../../core/hunks.ts";
import {
  coercePartials,
  findStackIdByBranch,
  getStackManifest,
  listStackManifests,
  putStackManifest,
  readWtState,
  validateStackManifest,
} from "../../core/wtstate.ts";
import { layoutStack, STACK_CONNECTOR } from "../../core/stack-layout.ts";
import { run as sizeRun } from "./size.ts";
import { blue, bold, cyan, dim, green, magenta, red, yellow } from "../colors.ts";

const HELP = `usage: wt stack <subcommand> [options]

subcommands:
  hunks [--holistic <b>] [--unified <n>] <file>...   list a file's holistic-diff
                             hunk ids (for hunk-level slice partitions; --json
                             for /split). --unified pins the diff context (the
                             stack's hunkContext, else 3); 0 splits coalesced edits
  apply <stackId>            materialize an already-ingested manifest
  apply --from <file>        strict-validate + ingest a manifest, then materialize
  plan --from <file>         strict-validate + ingest only (no materialize); prints stackId
  status [stackId]           render the manifest DAG + drift vs reality (defaults
                             to the current branch's stack; --all for every stack)
  context                    read-only pre-split context for /split (branch, base
                             decision, changed files, wt size); runs in the cwd
                             worktree, not the main clone
  section <stackId> <sliceIdOrPr> [label]   print one slice's static PR-body
                             "Stack" section (flat list, or a tree for a fork)
  split <stackId> <sliceId> --from <frag>   reshape: replace an open slice with N
                             sub-slices (re-threads descendants). Manifest only;
                             prints the apply/replay/retire next steps (or --apply
                             [--verify] to chain reshape → apply → replay)
  add [<branch>] [<stackId>] append an EXISTING branch to a live stack as a new
                             tip slice (adopts its open PR, or opens a draft PR);
                             never creates branches/worktrees — \`wt new\` does that
  reconcile [stackId]        manifest bookkeeping only: mark merged PRs, reparent
                             children (incl. a landed external/stack-on-stack parent)
  replay [stackId]           squash-safe replay each slice onto its parent (+ retarget PRs)
  rebase [stackId]           reconcile then replay (the one-shot /restack does)
                             (stackId defaults to the current branch's stack)
  prune-backups [--days N]   delete backup/restack-* + backup/stack-sync-*
                             branches older than N days (default 0 — all; the
                             commits stay recoverable via the reflog)

apply options:
  --from <file>              ingest a skill-authored manifest JSON (strict validation)
  --install                  run install per slice (default off — slices are install-free)
  --verify                   typecheck each cumulative slice prefix in a throwaway
                             worktree before opening any PR (needs [stack]
                             verify_command; aborts on the first red prefix)
split options:
  --from <file>              fragment JSON: array of { id, title, branch, files[] } sub-slices
  --plan                     preview the reshape without writing
  --apply                    chain reshape → apply → replay (still prints the PR-retire step)
  --verify                   with --apply: typecheck each new sub-slice prefix in a
                             throwaway worktree before opening any PR (needs [stack]
                             verify_command; aborts the chain on the first red prefix)
add options:
  (branch defaults to the current worktree's branch; a positional with a "/" is
   the branch, without is the stackId — resolved from --onto's branch when omitted)
  --onto <sliceId|branch>    parent to stack on (default: the fork base recorded
                             by \`wt new --base\` when it names a live slice, else
                             the highest-ordinal live slice; pass
                             ${config.branch.base} to root a new parallel lane)
  --title <t>                slice title (default: the PR's title, else derived
                             from the branch name)
status options:
  --json                     machine-readable output
  --all                      every stack manifest (default: the current branch's
                             stack, or all stacks when cwd is in no stack)
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
  let verify = false;
  let from: string | undefined;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]!;
    if (a === "--install") install = true;
    else if (a === "--verify") verify = true;
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
    console.error(red("usage: wt stack apply <stackId> | --from <manifest.json> [--install] [--verify]"));
    return 2;
  }
  const result = await applyStack(stackId, { install, verify }, logLine);
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

/**
 * Connector color for a forked lane (mirrors `laneColor` in the TUI theme,
 * with ansi instead of hex). Lane 0 — the main spine and every linear
 * stack — stays dim; forked siblings each pick a distinct hue. Avoids
 * green/red so a lane tint never reads as a status/drift marker.
 */
const CLI_LANE_PALETTE = [magenta, cyan, blue, yellow] as const;
function laneColor(lane: number, glyph: string): string {
  if (lane <= 0) return dim(glyph);
  return CLI_LANE_PALETTE[(lane - 1) % CLI_LANE_PALETTE.length]!(glyph);
}

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
  // Real-diff coverage gate, run here too (apply enforces it again) so a
  // mis-partitioned manifest — classically a rename whose delete-half no slice
  // claims — is caught at plan time, before any branch or PR is created.
  const manifest = getStackManifest(ingested.stackId);
  if (manifest) {
    const coverageError = await validateFileCoverage(
      manifest,
      config.paths.mainClone,
      new Map(),
    );
    if (coverageError) {
      console.error(red(`whole-file coverage check failed: ${coverageError}`));
      return 1;
    }
  }
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

/**
 * Read + structurally validate a `split` fragment: an array of sub-slice
 * specs (or `{ "into": [...] }`). Deep manifest validation happens in
 * `splitStack` (which runs the reshaped manifest through
 * `validateStackManifest`); this just guards the file shape.
 */
function readFragment(file: string): SubSliceSpec[] | null {
  let text: string;
  try {
    if (!statSync(file).isFile()) {
      console.error(red(`not a file: ${file}`));
      return null;
    }
    text = readFileSync(file, "utf8");
  } catch (e) {
    console.error(red(`cannot read ${file}: ${e instanceof Error ? e.message : String(e)}`));
    return null;
  }
  let parsed: unknown;
  try {
    parsed = JSON.parse(text);
  } catch (e) {
    console.error(red(`invalid JSON in ${file}: ${e instanceof Error ? e.message : String(e)}`));
    return null;
  }
  const arr = Array.isArray(parsed)
    ? parsed
    : parsed && typeof parsed === "object" && Array.isArray((parsed as Record<string, unknown>).into)
      ? ((parsed as Record<string, unknown>).into as unknown[])
      : null;
  if (!arr) {
    console.error(red(`fragment must be an array of sub-slices or { "into": [...] }`));
    return null;
  }
  const specs: SubSliceSpec[] = [];
  const errs: string[] = [];
  arr.forEach((v, i) => {
    if (!v || typeof v !== "object") {
      errs.push(`into[${i}]: not an object`);
      return;
    }
    const r = v as Record<string, unknown>;
    if (typeof r.id !== "string" || r.id.trim() === "") errs.push(`into[${i}]: "id" required`);
    if (typeof r.branch !== "string" || r.branch.trim() === "") errs.push(`into[${i}]: "branch" required`);
    const files = Array.isArray(r.files)
      ? r.files.filter((f): f is string => typeof f === "string" && f.trim() !== "")
      : [];
    // Shared lenient coercion (same shape the schema read path uses);
    // `validateStackManifest` is the strict net on the reshaped manifest.
    const partials = coercePartials(r.partials);
    // A sub-slice must own something — whole files or hunks. The strict
    // `validateStackManifest` re-checks the reshaped manifest as the net.
    if (files.length === 0 && partials.length === 0) {
      errs.push(`into[${i}]: needs a non-empty "files" or "partials"`);
    }
    if (typeof r.id !== "string" || typeof r.branch !== "string") return;
    specs.push({
      id: r.id,
      title: typeof r.title === "string" ? r.title : r.id,
      branch: r.branch,
      files,
      ...(partials.length > 0 ? { partials } : {}),
      oversized: r.oversized === true,
      ...(typeof r.oversizedReason === "string" ? { oversizedReason: r.oversizedReason } : {}),
    });
  });
  if (errs.length > 0) {
    console.error(red(`fragment validation failed:`));
    for (const e of errs) console.error(red(`  • ${e}`));
    return null;
  }
  return specs;
}

async function runSplit(argv: string[]): Promise<number> {
  let stackId: string | undefined;
  let sliceId: string | undefined;
  let from: string | undefined;
  let plan = false;
  let apply = false;
  let verify = false;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]!;
    if (a === "--plan") plan = true;
    else if (a === "--apply") apply = true;
    else if (a === "--verify") verify = true;
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
    else if (!sliceId) sliceId = a;
    else {
      console.error(red(`unexpected arg: ${a}`));
      return 2;
    }
  }
  if (plan && apply) {
    console.error(red("--plan and --apply are mutually exclusive"));
    return 2;
  }
  if (verify && !apply) {
    console.error(red("--verify only applies with --apply (it gates the chained apply)"));
    return 2;
  }
  if (!stackId || !sliceId || !from) {
    console.error(red("usage: wt stack split <stackId> <sliceId> --from <fragment.json> [--plan]"));
    return 2;
  }
  const fragment = readFragment(from);
  if (!fragment) return 1;
  const res = splitStack(stackId, sliceId, fragment, { plan });
  if (!res.ok) {
    console.error(red(res.error));
    return 1;
  }
  console.log(
    green(`${plan ? "would reshape" : "✓ reshaped"} ${bold(stackId)}: ${bold(sliceId)} → ${res.newSliceIds.join(", ")}`),
  );
  for (const s of res.slices) {
    const mark = res.newSliceIds.includes(s.id) ? cyan(" «new»") : "";
    console.log(`  ${dim(String(s.ordinal).padStart(2, "0"))} ${s.id}  ${dim("base:")} ${s.base}${mark}`);
  }
  console.log(dim(`\nsource for new slices: ${res.sourceBranch}`));
  if (res.rethreadedChildren.length > 0) {
    console.log(dim(`re-threaded onto new tip: ${res.rethreadedChildren.join(", ")}`));
  }
  if (plan) {
    console.log(dim("(--plan: nothing written)"));
    return 0;
  }

  const retire = res.supersededPr
    ? `gh pr close ${res.supersededPr} --delete-branch --comment "superseded by re-split"`
    : `git push origin --delete ${res.supersededBranch}`;

  // The split changed the slice SET, so every re-threaded descendant's PR body
  // still lists the OLD set (the now-superseded PR, none of the new sub-slices).
  // wt never authors PR bodies (that's `/split`'s job), so flag which bodies
  // are now stale rather than silently leaving them wrong.
  const staleBodies = [...res.newSliceIds.map((id) => `${id} (new)`), ...res.rethreadedChildren];
  if (staleBodies.length > 0) {
    console.log(
      yellow(`\n⚠ stale PR stack-sections — regenerate the bodies for: ${staleBodies.join(", ")}`),
    );
    console.log(dim("  (the slice set changed; descendant + new-slice stack sections list the old set)"));
  }

  if (!apply) {
    console.log(`\n${bold("next:")}`);
    console.log(`  1. wt stack apply ${stackId}     ${dim(`# materialize new sub-slices from ${res.sourceBranch}`)}`);
    console.log(`  2. wt stack replay ${stackId}    ${dim("# rebase + retarget the re-threaded descendants")}`);
    console.log(`  3. ${retire}  ${dim("# retire the split slice (after step 1)")}`);
    return 0;
  }

  // --apply: chain the mechanical happy-path (reshape → materialize → replay).
  // PR retirement stays an explicit printed step — it closes a PR + deletes a
  // branch, which wt won't do behind the user's back. With --verify the
  // chained apply typechecks each new sub-slice prefix BEFORE any PR opens —
  // the root-cause gate for a re-split whose ordering breaks compilation
  // (a sub-slice ordered ahead of the exhaustive consumer that needs it).
  console.log(
    `\n${bold("apply:")} materializing new sub-slices from ${res.sourceBranch}${verify ? " (verify)" : ""}`,
  );
  const applied = await applyStack(stackId, { install: false, verify }, logLine);
  if (applied.error) {
    console.error(red(applied.error));
    if (applied.materialized.length > 0) {
      console.error(dim(`(materialized before failure: ${applied.materialized.join(", ")})`));
    }
    return 1;
  }
  console.log(
    green(`✓ applied ${bold(stackId)} · ${applied.materialized.length} materialized, ${applied.skipped.length} skipped`),
  );

  console.log(`\n${bold("replay:")} rebasing re-threaded descendants onto the new tip`);
  const replayed = await replayStack(stackId, {}, logLine);
  const rc = reportReplayResult(stackId, "replayed", replayed);
  if (rc !== 0) return rc;

  console.log(`\n${bold("retire:")} ${retire}  ${dim("# close the superseded PR when ready")}`);
  return 0;
}

/**
 * `wt stack add [<branch>] [<stackId>] [--onto <sliceId|branch>] [--title <t>]`
 *
 * Positional disambiguation: branches in this workflow always carry a
 * namespace (`michael/…`), stackIds never do — so a positional with a `/` is
 * the branch, without is the stackId. The branch defaults to the current
 * worktree's HEAD (the common flow: `wt new --base <tip>`, work, then run
 * `add` from inside the new worktree). The stackId resolves from the cwd
 * branch when it's already a tracked slice, else from `--onto` when that
 * names a tracked branch — the new branch itself is in no manifest yet, so
 * from its own worktree you pass the stack via `--onto` or positionally.
 */
async function runAdd(argv: string[]): Promise<number> {
  let branch: string | undefined;
  let stackId: string | undefined;
  let onto: string | undefined;
  let title: string | undefined;
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]!;
    if (a === "--onto") {
      onto = argv[++i];
      if (!onto) {
        console.error(red("--onto requires a slice id or branch"));
        return 2;
      }
    } else if (a === "--title") {
      title = argv[++i];
      if (!title) {
        console.error(red("--title requires a value"));
        return 2;
      }
    } else if (a.startsWith("--")) {
      console.error(red(`unknown flag: ${a}`));
      return 2;
    } else if (a.includes("/")) {
      if (branch) {
        console.error(red(`two branch args: ${branch}, ${a}`));
        return 2;
      }
      branch = a;
    } else if (!stackId) stackId = a;
    else {
      console.error(red(`unexpected arg: ${a}`));
      return 2;
    }
  }
  if (!branch) {
    try {
      const head = (await git(["rev-parse", "--abbrev-ref", "HEAD"], process.cwd())).trim();
      if (head && head !== "HEAD") branch = head;
    } catch {
      // fall through to the usage error
    }
  }
  if (!branch) {
    console.error(red("no branch given and the current directory has no checked-out branch"));
    return 2;
  }
  if (!stackId) stackId = (await stackIdFromCwd()) ?? undefined;
  if (!stackId && onto?.includes("/")) stackId = findStackIdByBranch(onto) ?? undefined;
  if (!stackId) {
    console.error(
      red(
        "usage: wt stack add [<branch>] [<stackId>] [--onto <sliceId|branch>] [--title <t>]\n" +
          "  (couldn't resolve the target stack — pass <stackId>, or --onto a tracked branch)",
      ),
    );
    return 2;
  }

  const res = await addSliceToStack(stackId, branch, { onto, title }, logLine);
  if (!res.ok) {
    console.error(red(res.error));
    return 1;
  }
  const s = res.slice;
  console.log(
    green(
      `✓ added ${bold(s.id)} to ${bold(stackId)} — ${s.branch} onto ${res.parentBranch} (PR #${s.pr} ${res.prAction})`,
    ),
  );
  console.log(
    `  ${dim(String(s.ordinal).padStart(2, "0"))} ${s.title}  ${dim(`anchor ${s.baseSha?.slice(0, 9) ?? "?"}, ${s.files.length} file(s)${s.partials?.length ? ` + ${s.partials.length} partial` : ""}`)}`,
  );
  // Same staleness as a re-split, milder: existing PR bodies' stack sections
  // now list a slice set missing the new tip. wt never authors bodies.
  console.log(dim("  note: sibling PR stack-sections don't list the new slice — regenerate if you care"));
  return 0;
}

async function runPruneBackups(argv: string[]): Promise<number> {
  let days = 0;
  for (let i = 0; i < argv.length; i++) {
    if (argv[i] === "--days") {
      const n = Number(argv[++i]);
      if (!Number.isFinite(n) || n < 0) {
        console.error(red(`--days expects a non-negative number, got: ${argv[i] ?? "(nothing)"}`));
        return 2;
      }
      days = n;
    } else {
      console.error(red(`unknown prune-backups option: ${argv[i]}\n`));
      console.error(HELP);
      return 2;
    }
  }
  const res = await pruneStackBackups(days, logLine);
  if (res.deleted.length === 0 && res.kept.length === 0) {
    console.log(dim("no backup branches found"));
    return 0;
  }
  const keptNote = res.kept.length > 0 ? dim(` (${res.kept.length} kept)`) : "";
  console.log(green(`✓ deleted ${bold(String(res.deleted.length))} backup branch(es)`) + keptNote);
  return 0;
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

/**
 * List the canonical hunk ids of a file's holistic diff, so `/split` can
 * assign hunks to slices without re-implementing the content-hash scheme.
 * The base is the holistic branch's fork point from trunk — the SAME base
 * `materializeSliceCommit` reconstructs against, so ids line up.
 */
async function runHunks(rest: string[]): Promise<number> {
  let holistic = "";
  let json = false;
  let context: number | undefined;
  const files: string[] = [];
  for (let i = 0; i < rest.length; i++) {
    const a = rest[i]!;
    if (a === "--holistic") {
      const v = rest[++i];
      if (v === undefined || v.startsWith("--")) {
        console.error(red("--holistic needs a branch name"));
        return 2;
      }
      holistic = v;
    } else if (a === "--unified" || a === "-U") {
      const v = rest[++i];
      // Strict decimal only — `Number()` would quietly accept "", "0x4", "1e1".
      if (v === undefined || !/^\d+$/.test(v)) {
        console.error(red("--unified needs a non-negative integer"));
        return 2;
      }
      context = Number(v);
    } else if (a === "--json") json = true;
    else files.push(a);
  }
  if (files.length === 0) {
    console.error(red("usage: wt stack hunks [--holistic <branch>] [--unified <n>] [--json] <file>..."));
    return 2;
  }
  // Default the context from the resolved stack's pinned `hunkContext` so a
  // bare listing matches what `apply` will reconstruct against; an explicit
  // --unified always wins.
  let manifestContext: number | undefined;
  if (!holistic) {
    const stackId = await stackIdFromCwd();
    const manifest = stackId ? getStackManifest(stackId) : null;
    if (manifest?.holisticBranch) holistic = manifest.holisticBranch;
    manifestContext = manifest?.hunkContext;
  }
  if (!holistic) {
    console.error(red("no --holistic branch given and none resolvable from the current branch's stack"));
    return 2;
  }
  const effectiveContext = context ?? manifestContext ?? DEFAULT_HUNK_CONTEXT;
  const cwd = config.paths.mainClone;
  const base = await holisticBase(cwd, holistic);
  type HunkInfo = { id: string; header: string; added: number; removed: number };
  const out: Array<{ file: string; base: string; binary: boolean; hunks: HunkInfo[] }> = [];
  for (const file of files) {
    const fd = await fileHunks(cwd, base, holistic, file, effectiveContext);
    const hunks: HunkInfo[] = fd.hunks.map((h) => {
      const { added, removed } = hunkLineCounts(h);
      return { id: h.id, header: h.header, added, removed };
    });
    if (json) {
      out.push({ file, base, binary: fd.binary, hunks });
      continue;
    }
    if (fd.binary) {
      console.log(`${bold(file)} ${red("(binary — cannot hunk-split)")}`);
      continue;
    }
    if (hunks.length === 0) {
      console.log(`${bold(file)} ${dim("(no hunks)")}`);
      continue;
    }
    console.log(bold(file));
    for (const h of hunks) {
      console.log(`  ${cyan(h.id)}  ${green(`+${h.added}`)} ${red(`-${h.removed}`)}  ${dim(h.header)}`);
    }
  }
  if (json) console.log(JSON.stringify(out, null, 2));
  return 0;
}

/**
 * Read-only pre-split context, printed for the /split skill to consume.
 * Folds the old `split/scripts/context.sh`: fetch origin (so trunk isn't
 * stale right after a parent merge), then report branch / cleanliness /
 * the base decision / changed-file inventory / `wt size`. Runs against the
 * CURRENT directory (the holistic worktree), not the main clone.
 *
 * The base decision mirrors the recorded-fork-base precedence: a freshly
 * rebased parent chain puts HEAD on top of trunk too, so the bare
 * is-ancestor check would fold an unmerged parent's work into the slices —
 * consult `wt base` (the `wt new --base` record) first.
 */
async function runContext(_argv: string[]): Promise<number> {
  const cwd = process.cwd();
  // /split often runs right after merging a parent PR, when origin/<trunk>
  // is stale; a stale base folds already-merged work into the slices. Surface
  // a failed fetch so the base decision below isn't silently trusted offline.
  const fetchR = await gitRun(["fetch", "origin", "--quiet"], cwd);
  if (fetchR.exitCode !== 0) console.log("(git fetch failed — base may be stale)");

  const branchR = await gitRun(["rev-parse", "--abbrev-ref", "HEAD"], cwd);
  const branch = branchR.exitCode === 0 ? branchR.stdout.trim() : "?";
  let base = `origin/${config.branch.base}`;

  const statusR = await gitRun(["status", "--porcelain"], cwd);
  const clean = statusR.exitCode === 0 && statusR.stdout.trim() === "";
  console.log(`branch:     ${branch}`);
  console.log(`tree clean: ${clean ? "yes" : "NO"}`);

  const topR = await gitRun(["rev-parse", "--show-toplevel"], cwd);
  const slug = topR.exitCode === 0 ? basename(topR.stdout.trim()) : "";
  const rec = (slug && readWtState().slugs[slug]?.baseBranch) || "";
  const isAncestor = async (ref: string): Promise<boolean> =>
    ref.length > 0 &&
    (await gitRun(["merge-base", "--is-ancestor", ref, "HEAD"], cwd)).exitCode === 0;

  if (rec && (await isAncestor(rec))) {
    console.log(`recorded fork base: ${rec} — CONFIRMED (HEAD is on top of it).`);
    console.log("  -> stacked on this unmerged parent (case b). Use it as the base for the");
    console.log("     diff and every root slice; no need to ask.");
    base = rec;
  } else if (await isAncestor(base)) {
    console.log(`base status: OK — HEAD is on top of current ${base}`);
  } else {
    console.log(`base status: HEAD is NOT on current ${base}`);
    if (rec) console.log(`  recorded fork base: ${rec} — but HEAD is NOT on top of it (drifted?).`);
    console.log("  Do NOT assume. Either:");
    console.log(`  (a) main moved / a parent PR merged -> 'git rebase ${base}' (base = main)`);
    console.log("  (b) stacked on an unmerged parent    -> base = that parent branch");
    console.log("  The skill asks which before slicing (never silently rebase or split stale).");
  }
  console.log(`base:       ${base}`);

  const mb = await gitRun(["merge-base", base, "HEAD"], cwd);
  console.log(`merge-base: ${mb.exitCode === 0 ? mb.stdout.trim() : "?"}`);
  console.log("");
  console.log(`changed files (vs ${base}):`);
  const diff = await gitRun(["diff", "--stat", `${base}...HEAD`], cwd);
  if (diff.exitCode === 0 && diff.stdout.trim() !== "") {
    // mirror the old script's `tail -60`
    console.log(diff.stdout.replace(/\n+$/, "").split("\n").slice(-60).join("\n"));
  }
  console.log("");
  console.log("wt size:");
  // Size against the SAME resolved base as the inventory above — otherwise a
  // slice stacked on an unmerged parent would be measured vs trunk and fold the
  // parent's already-stacked work into the budget read.
  await sizeRun(["--base", base, "--json"]);
  return 0;
}

/**
 * Print the static PR-body "Stack" section for one slice (folds
 * `split/scripts/stack-section.{sh,py}`). Reads the manifest directly (no
 * gh round-trip): a linear stack renders as the flat ordinal list, a fork
 * as a nested bullet tree. Bare `#refs` only, so GitHub keeps the merge
 * status live and the section never needs maintaining. The leading blank
 * line before `---` is load-bearing (keeps a flush prose join from turning
 * the last paragraph into a setext H2).
 */
async function runSection(argv: string[]): Promise<number> {
  const [stackId, thisRef, ...labelParts] = argv;
  if (!stackId || !thisRef) {
    console.error(red("usage: wt stack section <stackId> <sliceIdOrPr> [label]"));
    return 2;
  }
  const manifest = getStackManifest(stackId);
  if (!manifest) {
    console.error(red(`no stack manifest: ${stackId}`));
    return 1;
  }
  const label = labelParts.join(" ") || stackId;
  const slices = [...manifest.slices].sort((a, b) => a.ordinal - b.ordinal);
  // Match a slice by its id or PR number; tolerate a `#`-prefixed PR arg.
  const wantPr = thisRef.replace(/^#/, "");
  const isThis = (s: (typeof slices)[number]): boolean =>
    s.id === thisRef || String(s.pr) === wantPr;
  const ref = (s: (typeof slices)[number]): string => (s.pr ? `#${s.pr}` : s.branch);

  // Build the slice tree from `base` ALONE (id or branch), deliberately NOT via
  // `layoutStack` (which also follows `dependsOn`). This mirrors the original
  // `stack-section.py` exactly so existing PR bodies stay byte-identical; the
  // PR-body tree and the TUI/status tree are intentionally separate renderers.
  // `base` is trunk, a sibling id, a sibling branch, or an external branch
  // (stack-on-stack root). In-stack parents resolve by id or branch; anything
  // else makes the slice a root.
  const byId = new Map(slices.map((s) => [s.id, s]));
  const byBranch = new Map(slices.map((s) => [s.branch, s]));
  const children = new Map<string, typeof slices>();
  const roots: typeof slices = [];
  for (const s of slices) {
    const parent = byId.get(s.base) ?? byBranch.get(s.base);
    if (parent && parent !== s) {
      const arr = children.get(parent.id) ?? [];
      arr.push(s);
      children.set(parent.id, arr);
    } else {
      roots.push(s);
    }
  }
  // Linear = one root, no slice with two children: rendered as the flat list.
  // `roots.length === 0` only happens on a malformed base cycle; fall back to
  // flat rather than render nothing.
  const linear = roots.length === 1 && [...children.values()].every((c) => c.length <= 1);

  const out: string[] = ["", "---", "", `Stack: **${label}**`, ""];
  if (linear || roots.length === 0) {
    for (const s of slices) {
      out.push(`${s.ordinal}. ${ref(s)}${isThis(s) ? " 👈" : ""}`);
    }
  } else {
    const seen = new Set<string>();
    const emit = (s: (typeof slices)[number], depth: number): void => {
      if (seen.has(s.id)) return;
      seen.add(s.id);
      out.push(`${"  ".repeat(depth)}- ${ref(s)}${isThis(s) ? " 👈" : ""}`);
      for (const c of (children.get(s.id) ?? []).sort((a, b) => a.ordinal - b.ordinal)) {
        emit(c, depth + 1);
      }
    };
    for (const r of roots) emit(r, 0);
    out.push("");
    out.push("*(nesting = stacks on, siblings = parallel)*");
  }
  console.log(out.join("\n"));
  return 0;
}

export async function run(argv: string[]): Promise<number> {
  const [sub, ...rest] = argv;
  if (!sub || sub === "--help" || sub === "-h") {
    console.log(HELP);
    return sub ? 0 : 2;
  }
  switch (sub) {
    case "hunks":
      return runHunks(rest);
    case "apply":
      return runApply(rest);
    case "plan":
      return runPlan(rest);
    case "status":
      return runStatus(rest);
    case "context":
      return runContext(rest);
    case "section":
      return runSection(rest);
    case "split":
      return runSplit(rest);
    case "add":
      return runAdd(rest);
    case "reconcile":
      return runReconcile(rest);
    case "replay":
      return runReplay(rest);
    case "rebase":
      return runRebase(rest);
    case "prune-backups":
      return runPruneBackups(rest);
    default:
      console.error(red(`unknown stack subcommand: ${sub}\n`));
      console.error(HELP);
      return 2;
  }
}
