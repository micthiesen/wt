import { expect, test } from "bun:test";
import { Effect } from "effect";
import { issueReaderArgs, readTrackerIssue } from "./issue-reader.ts";

test("reader substitutes an id literally into argv, without a shell", () => {
  expect(issueReaderArgs(["tracker", "read", "--id={id}", "{id}"], "ENG-1; touch nope"))
    .toEqual(["tracker", "read", "--id=ENG-1; touch nope", "ENG-1; touch nope"]);
});

test("reader uses destination cwd and preserves partial output and failure", async () => {
  const result = { stdout: "# Partial task\n", stderr: "attachment failed\n", exitCode: 7 };
  expect(await Effect.runPromise(readTrackerIssue(["tracker", "{id}"], "ENG-4", "/work/space", (args, opts) => {
    expect(args).toEqual(["tracker", "ENG-4"]);
    expect(opts).toEqual({ cwd: "/work/space", timeoutMs: 300_000 });
    return Effect.succeed(result);
  }))).toEqual(result);
});
