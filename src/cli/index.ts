// Static imports so the TS project includes all command modules in
// type-checking. Runtime dispatch is still keyed by command name below.
import * as lsCmd from "./commands/ls.ts";
import * as newCmd from "./commands/new.ts";
import * as rmCmd from "./commands/rm.ts";
import * as cleanCmd from "./commands/clean.ts";
import * as doctorCmd from "./commands/doctor.ts";
import * as stagesCmd from "./commands/stages.ts";
import * as logsCmd from "./commands/logs.ts";
import * as openCmd from "./commands/open.ts";
import * as destroyCmd from "./commands/_destroy.ts";

const HELP = `usage: wt <command> [options]

commands:
  ls           list all worktrees
  new         create a new worktree
  rm          remove a worktree
  clean       remove merged/gone worktrees
  doctor      report health of worktree(s)
  stages      list SST stages, optionally clean orphans
  logs        tail a destroy log
  open        open a worktree in Zed

Run \`wt <command> --help\` for per-command options where available.`;

type Runner = (argv: string[]) => Promise<number>;

const RUNNERS: Record<string, Runner> = {
  ls: lsCmd.run,
  new: newCmd.run,
  rm: rmCmd.run,
  clean: cleanCmd.run,
  doctor: doctorCmd.run,
  stages: stagesCmd.run,
  logs: logsCmd.run,
  open: openCmd.run,
  _destroy: destroyCmd.run,
};

export async function dispatch(argv: string[]): Promise<number> {
  const [cmd, ...rest] = argv;
  if (!cmd || cmd === "--help" || cmd === "-h") {
    console.log(HELP);
    return cmd ? 0 : 1;
  }
  const run = RUNNERS[cmd];
  if (!run) {
    console.error(`unknown command: ${cmd}\n`);
    console.error(HELP);
    return 2;
  }
  return run(rest);
}
