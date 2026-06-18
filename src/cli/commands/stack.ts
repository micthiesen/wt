import { readFileSync, statSync } from "node:fs";

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
  type RebaseResult,
  type StackStatusReport,
  type SubSliceSpec,
} from "../../core/stack-ops.ts";
import { git } from "../../core/git.ts";
import { DEFAULT_HUNK_CONTEXT, fileHunks, holisticBase, hunkLineCounts } from "../../core/hunks.ts";
import {
  coercePartials,
  findStackIdByBranch,
  getStackManifest,
  listStackManifests,
  putStackManifest,
  validateStackManifest,
} from "../../core/wtstate.ts";
import { bold, cyan, dim, green, red, yellow } from "../colors.ts";

const HELP = `usage: wt stack <subcommand> [options]

subcommands:
  hunks [--holistic <b>] [--unified <n>] <file>...   list a file's holistic-diff
                             hunk ids (for hunk-level slice partitions; --json
                             for /split). --unified pins the diff context (the
                             stack's hunkContext, else 3); 0 splits coalesced edits
  apply <stackId>            materialize an already-ingested manifest
  apply --from <file>        strict-validate + ingest a manifest, then materialize
  plan --from <file>         strict-validate + ingest only (no materialize); prints stackId
  status [stackId]           render the manifest DAG + drift vs reality
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
