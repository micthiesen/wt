import { describe, expect, test } from "bun:test";
import { Effect } from "effect";

import { run } from "./issue.ts";

async function runQuiet(argv: string[]): Promise<number> {
  const log = console.log;
  const error = console.error;
  console.log = (): void => {};
  console.error = (): void => {};
  try {
    return await Effect.runPromise(run(argv));
  } finally {
    console.log = log;
    console.error = error;
  }
}

describe("wt issue arguments", () => {
  for (const argv of [
    ["slug", "--id", "COZ-1", "extra"],
    ["slug", "--no-id", "extra"],
    ["slug", "--clear-id", "extra"],
    ["slug", "--gh", "1", "extra"],
    ["slug", "--clear-gh", "extra"],
    ["slug", "--read", "extra"],
    ["--read", "extra"],
    ["slug", "--unknown"],
  ]) {
    test(`rejects trailing arguments: ${argv.join(" ")}`, async () => {
      expect(await runQuiet(argv)).toBe(2);
    });
  }
});
