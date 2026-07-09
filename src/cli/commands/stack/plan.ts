import { bold, green, red } from "../../colors.ts";
import { ingestManifest } from "./manifest-io.ts";

export async function runPlan(argv: string[]): Promise<number> {
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
  const ingested = await ingestManifest(from);
  if (!ingested.ok) return 1;
  console.log(
    green(
      `✓ ingested ${bold(ingested.stackId)} (${ingested.sliceCount} slices) — run \`wt stack apply ${ingested.stackId}\` to materialize`,
    ),
  );
  return 0;
}
