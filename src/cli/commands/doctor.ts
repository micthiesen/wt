import { existsSync, readFileSync, statSync } from "node:fs";
import { join, resolve } from "node:path";

import { config } from "../../core/config.ts";
import { branchIsMerged, gitQuiet } from "../../core/git.ts";
import { fetchPrs } from "../../core/github.ts";
import { humanAge, lockAge, lockLabel, lockStatus } from "../../core/locks.ts";
import { run as sh } from "../../core/proc.ts";
import { computeStage } from "../../core/stage.ts";
import type { Check, CheckStatus, Worktree } from "../../core/types.ts";
import { isDeployed, listWorktrees } from "../../core/worktree.ts";
import { bold, cyan, dim, green, red, yellow } from "../colors.ts";
import {
  renderPrCell,
  renderSlugCell,
  renderStageCell,
  renderTable,
} from "../render.ts";

const STATUS_RANK: Record<CheckStatus, number> = { ok: 0, info: 0, warn: 1, err: 2 };
function worst(statuses: CheckStatus[]): CheckStatus {
  if (statuses.length === 0) return "ok";
  return statuses.reduce((a, b) => (STATUS_RANK[b] > STATUS_RANK[a] ? b : a));
}

const MARKERS: Record<CheckStatus, string> = {
  ok: green("✓"),
  info: cyan("·"),
  warn: yellow("⚠"),
  err: red("✗"),
};

function mkCheck(
  name: string,
  status: CheckStatus,
  message: string,
  detail: string[] = [],
): Check {
  return { name, status, message, detail };
}

async function checkWorkingTree(wt: Worktree): Promise<Check> {
  const r = await sh(["git", "status", "--porcelain"], { cwd: wt.path });
  if (r.exitCode !== 0) {
    return mkCheck("working tree", "err", `git status failed: ${r.stderr.trim()}`);
  }
  const out = r.stdout;
  if (!out.trim()) return mkCheck("working tree", "ok", "clean");
  const lines = out.split("\n").filter(Boolean);
  return mkCheck(
    "working tree",
    "warn",
    `${lines.length} uncommitted change(s)`,
    lines.slice(0, 10),
  );
}

async function checkSync(wt: Worktree): Promise<Check> {
  const r = await sh(
    ["git", "rev-list", "--left-right", "--count", `origin/${config.branch.base}...HEAD`],
    { cwd: wt.path },
  );
  if (r.exitCode !== 0) return mkCheck("sync", "warn", "cannot compare to origin/main");
  const parts = r.stdout.trim().split(/\s+/);
  const behind = parseInt(parts[0] ?? "0", 10);
  const ahead = parseInt(parts[1] ?? "0", 10);
  let unpushed = 0;
  const upstreamR = await sh(["git", "rev-parse", "--abbrev-ref", "@{u}"], { cwd: wt.path });
  if (upstreamR.exitCode === 0) {
    const cr = await sh(["git", "rev-list", "--count", "@{u}..HEAD"], { cwd: wt.path });
    if (cr.exitCode === 0) unpushed = parseInt(cr.stdout.trim(), 10) || 0;
  } else {
    // No upstream: fall back to origin/<branch> if present, else ahead-of-main.
    const branchR = await sh(["git", "rev-parse", "--abbrev-ref", "HEAD"], { cwd: wt.path });
    const branch = branchR.stdout.trim();
    if (branch && branch !== "HEAD") {
      const hasRemote = await gitQuiet(
        ["show-ref", "--verify", "--quiet", `refs/remotes/origin/${branch}`],
        wt.path,
      );
      if (hasRemote) {
        const cr = await sh(["git", "rev-list", "--count", `origin/${branch}..HEAD`], {
          cwd: wt.path,
        });
        if (cr.exitCode === 0) unpushed = parseInt(cr.stdout.trim(), 10) || 0;
      } else {
        unpushed = ahead;
      }
    }
  }

  const bits: string[] = [];
  if (ahead) bits.push(`${ahead} ahead of origin/main`);
  if (behind) bits.push(`${behind} behind origin/main`);
  if (unpushed) bits.push(`${unpushed} unpushed`);
  if (bits.length === 0) return mkCheck("sync", "ok", "up to date");
  const status: CheckStatus = behind || unpushed ? "warn" : "info";
  return mkCheck("sync", status, bits.join("; "));
}

async function checkSstStage(wt: Worktree): Promise<Check> {
  const stageFile = join(wt.path, ".sst", "stage");
  if (!existsSync(stageFile)) return mkCheck("sst stage", "warn", "no .sst/stage pinned");
  let actual = "";
  try {
    actual = readFileSync(stageFile, "utf8").trim();
  } catch {
    return mkCheck("sst stage", "warn", "cannot read .sst/stage");
  }
  const expected = computeStage(wt.slug);
  if (actual === expected) return mkCheck("sst stage", "ok", `pinned to ${actual}`);
  return mkCheck("sst stage", "warn", `stage=${JSON.stringify(actual)}, expected ${JSON.stringify(expected)}`);
}

async function checkSstDeploy(wt: Worktree): Promise<Check> {
  const outputs = join(wt.path, ".sst", "outputs.json");
  if (!isDeployed(wt.path)) {
    if (existsSync(outputs)) return mkCheck("sst deploy", "info", "destroyed (outputs.json empty)");
    return mkCheck("sst deploy", "info", "never deployed");
  }
  try {
    const st = statSync(outputs);
    const age = (Date.now() - st.mtimeMs) / 1000;
    return mkCheck("sst deploy", "info", `deployed (${humanAge(age)} ago)`);
  } catch {
    return mkCheck("sst deploy", "info", "deployed");
  }
}

async function checkNodeModules(wt: Worktree): Promise<Check> {
  const nm = join(wt.path, "node_modules");
  const store = join(nm, ".pnpm");
  if (!existsSync(nm) || !existsSync(store)) {
    return mkCheck("node_modules", "warn", "not installed — run `pnpm install`");
  }
  return mkCheck("node_modules", "ok", "installed");
}

async function checkLock(wt: Worktree): Promise<Check> {
  const info = lockStatus(wt.slug);
  if (!info) return mkCheck("lock", "ok", "none");
  const label = lockLabel(info);
  const pid = info.pid ?? "?";
  const age = lockAge(info);
  const suffix = age ? `, ${age} ago` : "";
  return mkCheck("lock", "warn", `${label} (pid ${pid}${suffix})`);
}

async function checkMerged(wt: Worktree): Promise<Check> {
  if (!wt.branch) return mkCheck("merged", "info", "no branch");
  if (await branchIsMerged(wt.branch))
    return mkCheck("merged", "info", "merged into origin/main");
  return mkCheck("merged", "ok", "not merged into origin/main");
}

async function checkPr(wt: Worktree): Promise<Check> {
  if (!wt.branch) return mkCheck("pr", "info", "no branch");
  const which = await sh(["which", "gh"]);
  if (which.exitCode !== 0) return mkCheck("pr", "info", "gh not installed");
  const r = await sh(
    ["gh", "pr", "view", wt.branch, "--json", "number,state,isDraft,url,statusCheckRollup"],
    { cwd: wt.path, timeoutMs: 10_000 },
  );
  if (r.exitCode !== 0) return mkCheck("pr", "info", "no PR");
  let data: Record<string, unknown>;
  try {
    data = JSON.parse(r.stdout);
  } catch {
    return mkCheck("pr", "warn", "gh returned non-JSON");
  }
  const state = (data.state as string) || "UNKNOWN";
  const draft = Boolean(data.isDraft);
  const num = data.number;
  const checks = (data.statusCheckRollup as Record<string, string>[] | undefined) ?? [];
  const failed = checks.filter((c) =>
    ["FAILURE", "CANCELLED", "TIMED_OUT"].includes((c.conclusion ?? "").toUpperCase()),
  );
  const pending = checks.filter((c) =>
    ["IN_PROGRESS", "QUEUED"].includes((c.status ?? "").toUpperCase()),
  );
  const parts: string[] = [`#${num}`, state.toLowerCase()];
  if (draft && state === "OPEN") parts.push("(draft)");
  if (failed.length) parts.push(`${failed.length} CI failing`);
  else if (pending.length) parts.push(`${pending.length} CI pending`);
  else parts.push("CI ok");
  let status: CheckStatus = "ok";
  if (failed.length) status = "err";
  else if (pending.length) status = "info";
  else if (state === "MERGED") status = "ok";
  return mkCheck("pr", status, parts.join(" "));
}

async function checkMainClone(): Promise<Check> {
  const main = config.paths.mainClone;
  const base = config.branch.base;
  const r = await sh(["git", "symbolic-ref", "--quiet", "--short", "HEAD"], {
    cwd: main,
  });
  if (r.exitCode !== 0) {
    return mkCheck(
      "main clone",
      "err",
      `detached HEAD in ${main} — should be on ${base}`,
    );
  }
  const head = r.stdout.trim();
  if (head !== base) {
    return mkCheck(
      "main clone",
      "err",
      `on branch ${JSON.stringify(head)} — should be on ${base}. ` +
        `Move that work into a worktree (\`wt new ${head}\`) and ` +
        `\`git -C ${main} checkout ${base}\`.`,
    );
  }
  return mkCheck("main clone", "ok", `on ${base}`);
}

async function runAllChecks(wt: Worktree, includePr: boolean): Promise<Check[]> {
  const tasks: Promise<Check>[] = [
    checkWorkingTree(wt),
    checkSync(wt),
    checkSstStage(wt),
    checkSstDeploy(wt),
    checkNodeModules(wt),
    checkLock(wt),
  ];
  if (includePr) tasks.push(checkPr(wt));
  tasks.push(checkMerged(wt));
  return Promise.all(tasks);
}

function currentWorktree(wts: Worktree[]): Worktree | null {
  const cwd = resolve(process.cwd());
  for (const w of wts) {
    const wp = resolve(w.path);
    if (cwd === wp || cwd.startsWith(wp + "/")) return w;
  }
  return null;
}

function wtToDict(wt: Worktree, checks: Check[]) {
  return {
    slug: wt.slug,
    branch: wt.branch,
    stage: wt.stage,
    path: wt.path,
    overall: worst(checks.map((c) => c.status)),
    checks,
  };
}

function renderMainCloneBanner(c: Check): void {
  if (c.status === "ok") return;
  console.log(`  ${MARKERS[c.status]}  ${bold(c.name.padEnd(14))} ${c.message}`);
}

async function reportOne(wt: Worktree, jsonOut: boolean): Promise<void> {
  const [mainBanner, checks] = await Promise.all([
    jsonOut ? Promise.resolve(null) : checkMainClone(),
    runAllChecks(wt, true),
  ]);
  if (jsonOut) {
    console.log(JSON.stringify(wtToDict(wt, checks), null, 2));
    return;
  }
  if (mainBanner) renderMainCloneBanner(mainBanner);
  console.log(`${bold("doctor")} · ${cyan(wt.slug)} ${dim(wt.branch)}`);
  for (const c of checks) {
    console.log(`  ${MARKERS[c.status]}  ${bold(c.name.padEnd(14))} ${c.message}`);
    for (const d of c.detail) console.log(`       ${dim(d)}`);
  }
  const overall = worst(checks.map((c) => c.status));
  console.log();
  console.log(`  ${MARKERS[overall]}  overall: ${bold(overall)}`);
  console.log(`     ${dim(`path:  ${wt.path}`)}`);
  console.log(`     ${dim(`stage: ${wt.stage}`)}`);
}

async function reportSummary(wts: Worktree[], jsonOut: boolean): Promise<void> {
  const skipPrs = jsonOut;
  const [prs, mainCheck, allChecks] = await Promise.all([
    skipPrs ? Promise.resolve(new Map()) : fetchPrs(),
    jsonOut ? Promise.resolve(null) : checkMainClone(),
    Promise.all(wts.map((w) => runAllChecks(w, false))),
  ]);
  if (jsonOut) {
    const out = wts.map((w, i) => wtToDict(w, allChecks[i]!));
    console.log(JSON.stringify(out, null, 2));
    return;
  }
  if (mainCheck) renderMainCloneBanner(mainCheck);

  type Row = { wt: Worktree; checks: Check[] };
  const rows: Row[] = wts.map((wt, i) => ({ wt, checks: allChecks[i]! }));
  const table = renderTable(rows, [
    { header: "slug", getter: (r) => renderSlugCell((r as Row).wt) },
    { header: "stage", getter: (r) => renderStageCell((r as Row).wt) },
    { header: "pr", getter: (r) => renderPrCell((r as Row).wt, prs) },
    {
      header: "highlights",
      getter: (r) => {
        const note = (r as Row).checks.filter(
          (c) => (c.status === "warn" || c.status === "err") && c.name !== "pr",
        );
        if (!note.length) return dim("all good");
        return note.slice(0, 3).map((c) => `${c.name}: ${c.message}`).join(", ");
      },
    },
  ]);
  console.log(table);
}

type Flags = { slug?: string; all: boolean; json: boolean };

function parse(argv: string[]): Flags | { error: string } {
  let slug: string | undefined;
  let all = false;
  let json = false;
  for (const a of argv) {
    if (a === "--all" || a === "-a") all = true;
    else if (a === "--json") json = true;
    else if (a.startsWith("--") || a.startsWith("-")) return { error: `unknown flag: ${a}` };
    else if (!slug) slug = a;
    else return { error: `unexpected arg: ${a}` };
  }
  return { slug, all, json };
}

export async function run(argv: string[]): Promise<number> {
  const parsed = parse(argv);
  if ("error" in parsed) {
    console.error(red(parsed.error));
    return 2;
  }
  const wtsAll = (await listWorktrees()).filter((w) => !w.isMain);
  if (wtsAll.length === 0) {
    console.log(dim("No worktrees."));
    return 0;
  }
  if (parsed.slug) {
    const target = wtsAll.find((w) => w.slug === parsed.slug);
    if (!target) {
      console.error(red(`No worktree with slug: ${parsed.slug}`));
      return 1;
    }
    await reportOne(target, parsed.json);
    return 0;
  }
  const here = parsed.all ? null : currentWorktree(wtsAll);
  if (here) {
    await reportOne(here, parsed.json);
    return 0;
  }
  await reportSummary(wtsAll, parsed.json);
  return 0;
}
