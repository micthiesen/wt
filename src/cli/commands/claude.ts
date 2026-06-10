import { dirSlug } from "../../core/stage.ts";
import {
  injectIntoSession,
  killSession,
  listSessions,
} from "../../core/tmux.ts";
import { listWorktrees } from "../../core/worktree.ts";
import type { Worktree } from "../../core/types.ts";
import { dim, green, red } from "../colors.ts";

const USAGE = `usage: wt claude send <slug> [text...]   type a prompt into the worktree's claude session
       wt claude ls                      list live claude sessions
       wt claude kill <slug>             kill the worktree's primary claude session

\`send\` upserts the worktree's PRIMARY Claude Code session: starts it
detached in the wt tmux server when absent (waiting for claude to
finish booting), pastes the text as if typed at the prompt, and
submits it. The prompt lands in the live conversation with its
existing context — not a headless \`claude -p\` run. Fire-and-forget:
there is no completion signal; attach via the TUI (F12) to watch.

With no [text...], stdin is read instead (heredoc-friendly for
multiline prompts). <slug> also accepts a branch name
(michael/eng-NNNN-...).`;

/** Resolve a slug-or-branch argument to a live (non-main) worktree. */
async function findWorktree(slugOrBranch: string): Promise<Worktree | null> {
  const slug = slugOrBranch.includes("/")
    ? dirSlug(slugOrBranch)
    : slugOrBranch;
  const wts = (await listWorktrees()).filter((w) => !w.isMain);
  return wts.find((w) => w.slug === slug) ?? null;
}

async function send(slugOrBranch: string, textArgs: string[]): Promise<number> {
  const wt = await findWorktree(slugOrBranch);
  if (!wt) {
    console.error(red(`no worktree: ${slugOrBranch}`));
    return 1;
  }
  const text = (
    textArgs.length > 0 ? textArgs.join(" ") : await Bun.stdin.text()
  ).trim();
  if (!text) {
    console.error(red("nothing to send — pass text args or pipe stdin"));
    return 2;
  }
  const res = await injectIntoSession({ slug: wt.slug, cwd: wt.path, text });
  if (!res.ok) {
    console.error(red(`inject failed: ${res.reason}`));
    return 1;
  }
  console.log(
    green(
      res.coldStarted
        ? `✓ started ${wt.slug}'s claude session and sent the prompt`
        : `✓ sent the prompt to ${wt.slug}'s claude session`,
    ),
  );
  console.log(dim("fire-and-forget — attach via the wt TUI (F12) to watch"));
  return 0;
}

async function ls(): Promise<number> {
  const sessions = await listSessions();
  if (sessions.claude.length === 0) {
    console.log(dim("no live claude sessions"));
    return 0;
  }
  for (const entry of [...sessions.claude].sort((a, b) =>
    a.slug.localeCompare(b.slug),
  )) {
    console.log(
      entry.name === null
        ? entry.slug
        : `${entry.slug}${dim(` ~${entry.name}`)}`,
    );
  }
  return 0;
}

async function kill(slugOrBranch: string): Promise<number> {
  const slug = slugOrBranch.includes("/")
    ? dirSlug(slugOrBranch)
    : slugOrBranch;
  const sessions = await listSessions();
  const live = sessions.claude.some((e) => e.slug === slug && e.name === null);
  if (!live) {
    console.log(dim(`${slug}: no live primary claude session`));
    return 0;
  }
  await killSession(slug);
  console.log(green(`✓ killed ${slug}'s primary claude session`));
  return 0;
}

export async function run(argv: string[]): Promise<number> {
  const [first, ...rest] = argv;
  if (!first || first === "--help" || first === "-h") {
    console.log(USAGE);
    return first ? 0 : 2;
  }
  if (first === "send") {
    const [slug, ...text] = rest;
    if (!slug) {
      console.error(red(USAGE));
      return 2;
    }
    return send(slug, text);
  }
  if (first === "ls") return ls();
  if (first === "kill") {
    const [slug] = rest;
    if (!slug) {
      console.error(red(USAGE));
      return 2;
    }
    return kill(slug);
  }
  console.error(red(USAGE));
  return 2;
}
