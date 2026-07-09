import { pruneStackBackups } from "../../../core/stack-ops.ts";
import { bold, dim, green, red } from "../../colors.ts";
import { HELP } from "./help.ts";
import { logLine } from "./shared.ts";

export async function runPruneBackups(argv: string[]): Promise<number> {
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
