import { applyStack } from "../../../core/stack-ops.ts";
import { bold, dim, green, red } from "../../colors.ts";
import { ingestManifest } from "./manifest-io.ts";
import { logLine } from "./shared.ts";

export async function runApply(argv: string[]): Promise<number> {
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
    const ingested = await ingestManifest(from);
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
