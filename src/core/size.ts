/**
 * The single canonical definition of "diff size" for wt. Every part of
 * the system that reports how big a change is (the `wt size` command,
 * the stack manifest's per-slice numbers, any advisory budget check)
 * reads from here so the number is identical everywhere.
 *
 * "Production line" deliberately EXCLUDES tests, snapshots, generated
 * files, and lockfiles — churn there isn't what draws review pushback,
 * and counting it would make the advisory budget lie. `isProductionPath`
 * is the one place that judgment lives; change it here and the whole
 * system moves together.
 */
import { join } from "node:path";

import { config } from "./config.ts";
import { run } from "./proc.ts";

const GIT_TIMEOUT_MS = 10_000;

const NEWLINE = 0x0a;

/** Path segments / suffixes that are NOT production code for sizing. */
const NON_PRODUCTION = [
  /(^|\/)__tests__\//,
  /(^|\/)__snapshots__\//,
  /(^|\/)__mocks__\//,
  /\.(?:spec|test)\.[cm]?[jt]sx?$/,
  /\.snap$/,
  /(^|\/)(?:pnpm-lock\.yaml|package-lock\.json|yarn\.lock|bun\.lock(?:b)?|Cargo\.lock|poetry\.lock|composer\.lock|Gemfile\.lock|go\.sum)$/,
  /\.min\.[cm]?js$/,
  /\.d\.ts$/,
  /(^|\/)(?:dist|build|generated|__generated__|\.next|out)\//,
];

/**
 * True when changes to `path` count toward the production-LOC total.
 * The canonical judgment — read this, never re-derive the exclusion
 * list inline.
 */
export function isProductionPath(path: string): boolean {
  return !NON_PRODUCTION.some((re) => re.test(path));
}

export type SizeFile = {
  path: string;
  added: number;
  removed: number;
  /** Whether this file's lines count toward `prodLines` / `prodFiles`. */
  production: boolean;
  /** True for git's binary marker (`-`); lines are 0 but the file still counts. */
  binary: boolean;
};

export type SizeReport = {
  /** added + removed across production files only. The headline number. */
  prodLines: number;
  /** Count of production files touched. */
  prodFiles: number;
  /** added + removed across every file (production + excluded). */
  totalLines: number;
  /** Total files touched. */
  files: number;
  perFile: SizeFile[];
};

export type SizeOptions = {
  /** Where to run git. Defaults to the main clone. */
  cwd?: string;
  /**
   * Comparison base ref. Defaults to `origin/<config.branch.base>`. The
   * diff is `<merge-base(base, target)>..<target>` so an advancing trunk
   * doesn't inflate the count — only this branch's own contribution.
   */
  base?: string;
  /**
   * Right-hand side of the diff. Defaults to the working tree (committed
   * + uncommitted + untracked). Pass a ref (e.g. a holistic branch) to
   * size a branch other than the checkout; untracked files are only
   * counted for the working-tree default.
   */
  target?: string;
  /** Restrict to these pathspecs (e.g. a slice's file list). */
  paths?: readonly string[];
};

/** Parse one `--numstat` row. Returns null for blank / malformed lines. */
function parseNumstatLine(line: string): SizeFile | null {
  if (!line) return null;
  // `<added>\t<removed>\t<path>` — added/removed are `-` for binaries.
  const m = line.match(/^(\d+|-)\t(\d+|-)\t(.+)$/);
  if (!m) return null;
  const binary = m[1] === "-" || m[2] === "-";
  const added = binary ? 0 : Number.parseInt(m[1]!, 10);
  const removed = binary ? 0 : Number.parseInt(m[2]!, 10);
  // Renames render as `old => new` or `pre/{old => new}/post`. Resolve
  // to the new path so classification reads the destination.
  const path = resolveRenamePath(m[3]!);
  return { path, added, removed, production: isProductionPath(path), binary };
}

function resolveRenamePath(raw: string): string {
  if (!raw.includes("=>")) return raw;
  const brace = raw.match(/^(.*)\{(.*) => (.*)\}(.*)$/);
  if (brace) {
    const [, pre, , to, post] = brace;
    return `${pre}${to}${post}`.replace(/\/\//g, "/");
  }
  const simple = raw.match(/^.* => (.*)$/);
  return simple ? simple[1]! : raw;
}

/**
 * New (untracked) files don't show up in `git diff`, so a working-tree
 * size that ignored them would badly undercount any change that adds
 * files. List them and count their lines as pure additions. Files with
 * a NUL byte are treated as binary (git's heuristic): counted as a
 * touched file with 0 lines.
 */
async function untrackedFiles(
  cwd: string,
  paths: readonly string[] | undefined,
): Promise<SizeFile[]> {
  // `-c core.quotePath=false` so non-ASCII paths arrive literal (UTF-8)
  // instead of octal-escaped + double-quoted, which would break
  // production-path classification.
  const args = ["git", "-c", "core.quotePath=false", "ls-files", "--others", "--exclude-standard"];
  if (paths && paths.length > 0) args.push("--", ...paths);
  const r = await run(args, { cwd, timeoutMs: GIT_TIMEOUT_MS });
  if (r.exitCode !== 0) return [];
  const rels = r.stdout.split("\n").filter((l) => l.length > 0);
  return Promise.all(
    rels.map(async (rel): Promise<SizeFile> => {
      const production = isProductionPath(rel);
      try {
        const bytes = new Uint8Array(await Bun.file(join(cwd, rel)).arrayBuffer());
        const probe = Math.min(bytes.length, 8000);
        for (let i = 0; i < probe; i++) {
          if (bytes[i] === 0) return { path: rel, added: 0, removed: 0, production, binary: true };
        }
        // Count lines the way numstat does: one per newline byte, plus a
        // trailing partial line when the file doesn't end in a newline.
        let added = 0;
        for (let i = 0; i < bytes.length; i++) if (bytes[i] === NEWLINE) added++;
        if (bytes.length > 0 && bytes[bytes.length - 1] !== NEWLINE) added++;
        return { path: rel, added, removed: 0, production, binary: false };
      } catch {
        // Unreadable (vanished mid-scan, perms) — count the file, 0 lines.
        return { path: rel, added: 0, removed: 0, production, binary: true };
      }
    }),
  );
}

/**
 * Compute the production-LOC + file-count size of a diff. The headline
 * is `prodLines` (added + removed across production files). Pure
 * read-only git; never throws on a missing merge-base — falls back to a
 * plain two-dot diff against the base. Untracked files are folded in
 * when sizing the working tree (no explicit `target`).
 */
export async function computeSize(opts: SizeOptions = {}): Promise<SizeReport> {
  const cwd = opts.cwd ?? config.paths.mainClone;
  const base = opts.base ?? `origin/${config.branch.base}`;
  const target = opts.target ?? "";

  // Anchor at the merge-base so a moved-ahead trunk doesn't read as this
  // branch's deletions. If there's no common ancestor, fall back to the
  // raw base ref.
  let left = base;
  const mb = await run(
    ["git", "merge-base", base, target || "HEAD"],
    { cwd, timeoutMs: GIT_TIMEOUT_MS },
  );
  if (mb.exitCode === 0 && mb.stdout.trim()) left = mb.stdout.trim();

  // `-c core.quotePath=false`: keep non-ASCII paths literal so the
  // numstat path field classifies correctly (see untrackedFiles).
  const args = ["git", "-c", "core.quotePath=false", "diff", "--numstat", left];
  if (target) args.push(target);
  if (opts.paths && opts.paths.length > 0) args.push("--", ...opts.paths);

  // Untracked files only exist relative to the working tree, so fold
  // them in only when there's no explicit ref target.
  const [diff, untracked] = await Promise.all([
    run(args, { cwd, timeoutMs: GIT_TIMEOUT_MS }),
    target ? Promise.resolve([] as SizeFile[]) : untrackedFiles(cwd, opts.paths),
  ]);

  const perFile: SizeFile[] = [];
  if (diff.exitCode === 0) {
    for (const line of diff.stdout.split("\n")) {
      const f = parseNumstatLine(line);
      if (f) perFile.push(f);
    }
  }
  perFile.push(...untracked);

  let prodLines = 0;
  let prodFiles = 0;
  let totalLines = 0;
  for (const f of perFile) {
    totalLines += f.added + f.removed;
    if (f.production) {
      prodLines += f.added + f.removed;
      prodFiles += 1;
    }
  }
  return { prodLines, prodFiles, totalLines, files: perFile.length, perFile };
}
