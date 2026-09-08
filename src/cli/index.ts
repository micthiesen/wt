import { Data, Effect } from "effect";

import { causeMessage } from "../core/errors.ts";

const HELP = `usage: wt <command> [options]

commands:
  init         create an isolated .wt.toml for a repository
  state        migrate durable repository state into SQLite
  ls           list all worktrees
  fleet       audit the fleet: asserted status vs session + PR reality
  new         create a new worktree
  rm          remove a worktree
  clean       remove merged/gone worktrees
  doctor      report health of worktree(s)
  stages      list SST stages, optionally clean orphans
  logs        tail a destroy log
  perf        one-shot perf snapshot: wt-downstream vs the rest of the machine
  open        open a worktree in your editor
  restack     rebase a stack of worktrees onto its updated parents
  skills      keep wt's agent skills + instructions installed and current
  update      update wt itself (fast-forward the source clone; \`update log\` for history)
  rollback    reset wt to the last version that booted healthy
  version     print the running wt version (git short hash)
  events      manage the optional GitHub webhook daemon
  remote      enter or run wt on the configured SSH remote
  base        show / set / clear a worktree's recorded fork base
  merge       arm "merge when ready" on a worktree's PR (queue-aware)
  status      show / assert a worktree's work status (agent-facing)
  edge        assert / list merge-order edges between worktrees (self-expiring)
  section     list / move / rename / drop the fleet's sections (the human's batching)
  manager     attach the fleet-coordinator session / send it a message / report a result
  issue       show a worktree's issue links / attach a GitHub issue (--gh)
  agent       send to / start a worktree's configured primary coding agent
  claude      drive a worktree's Claude Code session (send / ls / stop)
  dev         start / stop / inspect a worktree's [dev_server]

Run \`wt <command> --help\` for per-command options where available.`;

export type Runner = (argv: string[]) => Effect.Effect<number, Error>;
export type Loader = () => Promise<{ run: Runner }>;

export class CommandLoadError extends Data.TaggedError("CommandLoadError")<{
  readonly command: string;
  readonly cause: unknown;
}> {
  override get message(): string {
    return `${this.command}: could not load the command: ${causeMessage(this.cause)}`;
  }
}

export class CommandRunError extends Data.TaggedError("CommandRunError")<{
  readonly command: string;
  readonly cause: unknown;
}> {
  override get message(): string {
    return `${this.command}: ${causeMessage(this.cause)}`;
  }
}

export type DispatchError = CommandLoadError | CommandRunError;

/**
 * One lazy loader per command, so running `wt <cmd>` imports that
 * command's module graph and nothing else. This is a blast-radius rule,
 * not a micro-optimization: static imports here meant a broken module
 * anywhere under any command took out EVERY command, including the
 * `wt status` that agents use to report they're stuck. It also drops
 * ~60-100ms off the common commands (35 modules for `wt status` vs 153
 * for the whole barrel).
 *
 * Nothing is lost by not importing statically: tsconfig has no `include`,
 * so `tsc --noEmit` type-checks every module here whether or not this
 * file names it.
 */
const RUNNERS: Record<string, Loader> = {
  init: () => import("./commands/init.ts"),
  state: () => import("./commands/state.ts"),
  ls: () => import("./commands/ls.ts"),
  fleet: () => import("./commands/fleet.ts"),
  new: () => import("./commands/new.ts"),
  rm: () => import("./commands/rm.ts"),
  clean: () => import("./commands/clean.ts"),
  doctor: () => import("./commands/doctor.ts"),
  stages: () => import("./commands/stages.ts"),
  logs: () => import("./commands/logs.ts"),
  perf: () => import("./commands/perf.ts"),
  open: () => import("./commands/open.ts"),
  restack: () => import("./commands/restack.ts"),
  skills: () => import("./commands/skills.ts"),
  update: () => import("./commands/update.ts"),
  rollback: () => import("./commands/rollback.ts"),
  version: () => import("./commands/version.ts"),
  events: () => import("./commands/events.ts"),
  remote: () => import("./commands/remote.ts"),
  _remote: () => import("./commands/_remote.ts"),
  _hello: () => import("./commands/_hello.ts"),
  _snapshot: () => import("./commands/_snapshot.ts"),
  "_action-popup": () => import("./commands/_action-popup.tsx"),
  "_help-popup": () => import("./commands/_help-popup.tsx"),
  "_workspace-host": () => import("./commands/_workspace-host.ts"),
  _session: () => import("./commands/_session.ts"),
  base: () => import("./commands/base.ts"),
  merge: () => import("./commands/merge.ts"),
  status: () => import("./commands/status.ts"),
  edge: () => import("./commands/edge.ts"),
  section: () => import("./commands/section.ts"),
  manager: () => import("./commands/manager.ts"),
  issue: () => import("./commands/issue.ts"),
  agent: () => import("./commands/agent.ts"),
  claude: () => import("./commands/claude.ts"),
  dev: () => import("./commands/dev.ts"),
  _destroy: () => import("./commands/_destroy.ts"),
  "_dev-giveup": () => import("./commands/_dev-giveup.ts"),
  "_claude-hook": () => import("./commands/_claude-hook.ts"),
};

/**
 * Load one command module and run it, tagging load vs. run failures
 * distinctly. `dispatch` below uses this through the lazy-loader map.
 * main.ts's self-update family (`update`/`rollback`/`version`) keeps its
 * own copy of this shape on purpose: it must work when THIS module is
 * what a bad update broke, so it cannot import anything from here.
 */
export function loadAndRunCommand(
  command: string,
  load: Loader,
  argv: string[],
): Effect.Effect<number, CommandLoadError | CommandRunError> {
  return Effect.tryPromise({
    try: load,
    catch: (cause) => new CommandLoadError({ command, cause }),
  }).pipe(
    Effect.flatMap(({ run }) => {
      let result: Effect.Effect<number, Error>;
      try {
        result = run(argv);
      } catch (cause) {
        return Effect.fail(new CommandRunError({ command, cause }));
      }
      return result.pipe(
        Effect.mapError((cause) => new CommandRunError({ command, cause })),
      );
    }),
  );
}

/**
 * Effect-native command dispatcher. Dynamic imports remain per command so a
 * broken leaf cannot take down unrelated recovery/status commands.
 *
 */
export function dispatch(argv: string[]): Effect.Effect<number, DispatchError> {
  const [cmd, ...rest] = argv;
  if (cmd === "--help" || cmd === "-h") {
    return Effect.sync(() => {
      console.log(HELP);
      return 0;
    });
  }
  const command = cmd === "--version" || cmd === "-v" ? "version" : cmd;
  const load = command ? RUNNERS[command] : undefined;
  if (!command || !load) {
    return Effect.sync(() => {
      console.error(`unknown command: ${cmd ?? ""}\n`);
      console.error(HELP);
      return 2;
    });
  }
  return loadAndRunCommand(command, load, rest);
}
