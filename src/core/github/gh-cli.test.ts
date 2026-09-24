import { expect, test } from "bun:test";

import { ghFailureMessage, GH_TIMEOUT_MS } from "./gh-cli.ts";

test("reports an observed local timeout rather than the platform's SIGKILL exit code", () => {
  expect(ghFailureMessage({ stdout: "", stderr: "", exitCode: 137, timedOut: true }))
    .toBe(`gh timed out after ${GH_TIMEOUT_MS / 1000}s (including any local concurrency-gate wait); request outcome unknown`);
  expect(ghFailureMessage({ stdout: "", stderr: "", exitCode: 137 }))
    .toBe("gh exited 137");
});

test("preserves GitHub's useful error text", () => {
  expect(ghFailureMessage({ stdout: "", stderr: "gh: API rate limit exceeded\nmore detail", exitCode: 1 }))
    .toBe("gh: API rate limit exceeded\nmore detail");
  expect(ghFailureMessage({ stdout: "", stderr: "gh: API rate limit exceeded\nmore detail", exitCode: 1 }, true))
    .toBe("gh: API rate limit exceeded");
});
