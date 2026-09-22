import { describe, expect, test } from "bun:test";
import { Effect, Exit } from "effect";

import { acquireRuntimeResource, invalidateRefQueries } from "./runtime.tsx";

describe("TUI runtime resources", () => {
  test("refs invalidate branch tips used by branch.advanced", () => {
    const keys: unknown[][] = [];

    invalidateRefQueries((key) => keys.push([...key]));

    expect(keys).toContainEqual(["watchedBranchTips"]);
  });

  test("releases every acquired resource when later startup fails", async () => {
    const releases: string[] = [];
    const program = Effect.scoped(
      Effect.gen(function* () {
        yield* acquireRuntimeResource(
          Effect.succeed("watcher"),
          (name) => {
            releases.push(name);
          },
        );
        yield* acquireRuntimeResource(
          Effect.succeed("query-client"),
          (name) => {
            releases.push(name);
          },
        );
        return yield* acquireRuntimeResource(
          Effect.fail("renderer startup failed"),
          () => {
            releases.push("renderer");
          },
        );
      }),
    );

    const exit = await Effect.runPromiseExit(program);

    expect(Exit.isFailure(exit)).toBe(true);
    expect(releases).toEqual(["query-client", "watcher"]);
  });

  test("a throwing finalizer does not skip later cleanup", async () => {
    const releases: string[] = [];
    const program = Effect.scoped(
      Effect.gen(function* () {
        yield* acquireRuntimeResource(
          Effect.succeed("first"),
          (name) => {
            releases.push(name);
          },
        );
        yield* acquireRuntimeResource(
          Effect.succeed("throws"),
          (name) => {
            releases.push(name);
            throw new Error("cleanup failed");
          },
        );
        yield* acquireRuntimeResource(
          Effect.succeed("last"),
          (name) => {
            releases.push(name);
          },
        );
      }),
    );

    const exit = await Effect.runPromiseExit(program);

    expect(Exit.isSuccess(exit)).toBe(true);
    expect(releases).toEqual(["last", "throws", "first"]);
  });
});
