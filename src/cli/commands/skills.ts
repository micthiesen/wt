import {
  cpSync,
  existsSync,
  mkdirSync,
  readdirSync,
  readFileSync,
  renameSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

import { run as sh } from "../../core/proc.ts";
import { bold, cyan, dim, green, red, yellow } from "../colors.ts";

/** Bundled skills live at <repo>/skills/<name>/SKILL.md. */
const SKILLS_ROOT = join(import.meta.dir, "..", "..", "..", "skills");

const USAGE = `usage: wt skills install [--harness claude|codex|opencode] [--rulesync] [options] [<name>...]

Install wt's bundled workflow skills (split, restack, wt) into a harness's
skills directory. With no <name>, installs all of them.

modes (pick one):
  --harness <h>     copy into the harness's native skills dir, with clean
                    frontmatter (strips the rulesync-only \`targets:\` key):
                      claude    -> ~/.claude/skills/<name>
                      opencode  -> ~/.claude/skills/<name>  (OpenCode reads it)
                      codex     -> \$CODEX_HOME/skills/<name>  (default ~/.codex/skills)
  --rulesync        copy the source verbatim into a rulesync skills dir so an
                    existing rulesync pipeline fans it out to every harness
                    (default ~/.dotfiles/.rulesync/skills); pair with --build

options:
  --dest <dir>      override the destination skills dir (either mode)
  --build           after --rulesync, run the dotfiles rulesync generator
                    (<dotfiles>/scripts/rulesync.sh) to regenerate + stow

The same Claude-style source serves every harness: Claude/OpenCode auto-run a
skill's \`!\`…\`\` setup blocks; Codex reads them as plain commands to run.`;

type Mode =
  | { kind: "harness"; harness: string; dest: string }
  | { kind: "rulesync"; dest: string; build: boolean };

/**
 * Strip the rulesync-only `targets:` block from a SKILL.md's frontmatter so a
 * native (non-rulesync) install carries clean Claude/Codex frontmatter. No-op
 * when absent. Other keys (name, description, argument-hint, user_invocable)
 * are kept — Claude reads them and Codex tolerates them.
 */
function stripRulesyncKeys(md: string): string {
  const m = md.match(/^---\n([\s\S]*?)\n---\n([\s\S]*)$/);
  if (!m) return md;
  const out: string[] = [];
  let skipping = false;
  for (const line of m[1]!.split("\n")) {
    if (skipping) {
      // Still inside the (indented) targets list — drop it.
      if (/^\s+\S/.test(line)) continue;
      skipping = false;
    }
    if (/^targets:/.test(line)) {
      skipping = true;
      continue;
    }
    out.push(line);
  }
  return `---\n${out.join("\n")}\n---\n${m[2]}`;
}

function harnessDest(harness: string): string | null {
  const home = homedir();
  switch (harness) {
    case "claude":
    case "opencode":
      return join(home, ".claude", "skills");
    case "codex":
      return join(process.env.CODEX_HOME ?? join(home, ".codex"), "skills");
    default:
      return null;
  }
}

function bundledSkills(): string[] {
  if (!existsSync(SKILLS_ROOT)) return [];
  return readdirSync(SKILLS_ROOT, { withFileTypes: true })
    .filter((e) => e.isDirectory() && existsSync(join(SKILLS_ROOT, e.name, "SKILL.md")))
    .map((e) => e.name)
    .sort();
}

function installOne(
  name: string,
  destRoot: string,
  transform: (md: string) => string,
): void {
  const src = join(SKILLS_ROOT, name);
  const dest = join(destRoot, name);
  // Stage into a temp sibling, transform there, then swap into place: the
  // destination is only removed right before the rename, so a failed copy can't
  // leave the old skill deleted with nothing in its place, and the rename
  // replaces a symlinked dest rather than writing through it.
  const staged = join(destRoot, `.${name}.tmp-${process.pid}`);
  if (existsSync(staged)) rmSync(staged, { recursive: true, force: true });
  try {
    cpSync(src, staged, { recursive: true });
    const skillPath = join(staged, "SKILL.md");
    if (existsSync(skillPath)) {
      writeFileSync(skillPath, transform(readFileSync(skillPath, "utf8")));
    }
    // Overwrite: a re-install is an update. (The user opted into clobbering.)
    if (existsSync(dest)) rmSync(dest, { recursive: true, force: true });
    renameSync(staged, dest);
  } finally {
    if (existsSync(staged)) rmSync(staged, { recursive: true, force: true });
  }
}

function parse(argv: string[]): { error: string } | { names: string[]; mode: Mode } {
  let harness: string | undefined;
  let rulesync = false;
  let build = false;
  let destOverride: string | undefined;
  const names: string[] = [];
  for (let i = 0; i < argv.length; i++) {
    const a = argv[i]!;
    if (a === "--harness") {
      const v = argv[++i];
      if (!v) return { error: "--harness requires a value" };
      harness = v;
    } else if (a === "--rulesync") {
      rulesync = true;
    } else if (a === "--build") {
      build = true;
    } else if (a === "--dest") {
      const v = argv[++i];
      if (!v) return { error: "--dest requires a directory" };
      destOverride = v;
    } else if (a.startsWith("--")) {
      return { error: `unknown flag: ${a}` };
    } else {
      names.push(a);
    }
  }

  if (harness && rulesync) return { error: "pick one of --harness or --rulesync, not both" };
  if (harness && build) return { error: "--build only applies to --rulesync" };

  let mode: Mode;
  if (harness) {
    const dest = destOverride ?? harnessDest(harness);
    if (!dest) return { error: `unknown harness: ${harness} (claude|codex|opencode)` };
    mode = { kind: "harness", harness, dest };
  } else if (rulesync) {
    const dest = destOverride ?? join(homedir(), ".dotfiles", ".rulesync", "skills");
    mode = { kind: "rulesync", dest, build };
  } else {
    return { error: "__no_mode__" };
  }
  return { names, mode };
}

export async function run(argv: string[]): Promise<number> {
  const [sub, ...rest] = argv;
  if (!sub || sub === "--help" || sub === "-h") {
    console.log(USAGE);
    return sub ? 0 : 2;
  }
  if (sub !== "install") {
    console.error(red(`unknown skills subcommand: ${sub}\n`));
    console.error(USAGE);
    return 2;
  }

  const parsed = parse(rest);
  if ("error" in parsed) {
    if (parsed.error === "__no_mode__") {
      console.error(red("specify a mode: --harness <h> or --rulesync\n"));
      const rs = join(homedir(), ".dotfiles", ".rulesync", "skills");
      if (existsSync(rs)) {
        console.error(dim(`(detected a rulesync setup at ${rs} — \`wt skills install --rulesync\` installs through it)`));
      }
      console.error(USAGE);
      return 2;
    }
    console.error(red(parsed.error));
    return 2;
  }

  const { names, mode } = parsed;
  const available = bundledSkills();
  if (available.length === 0) {
    console.error(red(`no bundled skills found at ${SKILLS_ROOT}`));
    return 1;
  }
  const wanted = names.length > 0 ? names : available;
  const unknown = wanted.filter((n) => !available.includes(n));
  if (unknown.length > 0) {
    console.error(red(`unknown skill(s): ${unknown.join(", ")} (have: ${available.join(", ")})`));
    return 1;
  }

  try {
    mkdirSync(mode.dest, { recursive: true });
  } catch (e) {
    console.error(red(`cannot create ${mode.dest}: ${e instanceof Error ? e.message : String(e)}`));
    return 1;
  }
  const transform = mode.kind === "harness" ? stripRulesyncKeys : (md: string) => md;
  for (const name of wanted) {
    try {
      installOne(name, mode.dest, transform);
    } catch (e) {
      console.error(red(`failed to install ${name}: ${e instanceof Error ? e.message : String(e)}`));
      return 1;
    }
    console.log(`${green("✓")} ${bold(name)} ${dim("→")} ${join(mode.dest, name)}`);
  }

  if (mode.kind === "harness") {
    console.log(
      dim(
        mode.harness === "codex"
          ? "restart Codex to pick up new skills"
          : `installed for ${mode.harness}`,
      ),
    );
  }

  if (mode.kind === "rulesync" && mode.build) {
    const dotfiles = dirname(dirname(mode.dest)); // .rulesync/skills -> .rulesync -> root
    const script = join(dotfiles, "scripts", "rulesync.sh");
    if (!existsSync(script)) {
      console.error(yellow(`--build: no generator at ${script}; run your rulesync build manually`));
      return 0;
    }
    console.log(dim(`\nrunning ${script} …`));
    const r = await sh(["bash", script], { cwd: dotfiles });
    if (r.stdout.trim()) process.stdout.write(r.stdout.endsWith("\n") ? r.stdout : r.stdout + "\n");
    if (r.stderr.trim()) process.stderr.write(r.stderr.endsWith("\n") ? r.stderr : r.stderr + "\n");
    if (r.exitCode !== 0) {
      console.error(red(`rulesync build failed (exit ${r.exitCode})`));
      return 1;
    }
    console.log(green("✓ rulesync regenerated + stowed"));
  }

  console.log(`\n${cyan(String(wanted.length))} skill(s) installed.`);
  return 0;
}
