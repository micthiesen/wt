import { red } from "../../colors.ts";
import { runAdd } from "./add.ts";
import { runApply } from "./apply.ts";
import { runContext } from "./context.ts";
import { HELP } from "./help.ts";
import { runHunks } from "./hunks.ts";
import { runPlan } from "./plan.ts";
import { runPruneBackups } from "./prune-backups.ts";
import { runRebase, runReconcile, runReplay } from "./rebase-replay.ts";
import { runSection } from "./section.ts";
import { runSplit } from "./split.ts";
import { runStatus } from "./status.ts";

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
