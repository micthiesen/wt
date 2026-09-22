import { describe, expect, test } from "bun:test";
import { Effect } from "effect";
import { fetchIssueStatuses, issueStatusArgs, issueStatusIds, parseIssueStatuses } from "./issue-status.ts";

test("status IDs batch unique resolved overrides and respect an explicit none", () => {
  expect(issueStatusIds([
    { slug: "eng-2-first" },
    { slug: "eng-2-second" },
    { slug: "eng-3-old", issueId: "eng-1" },
    { slug: "eng-4-none", issueId: "" },
    { slug: "no-id" },
  ])).toEqual(["ENG-1", "ENG-2"]);
});

test("argv expansion passes IDs literally as separate arguments", () => {
  expect(issueStatusArgs(["tracker", "statuses", "{ids}", "--json"], ["ENG-1", "ENG-2; echo nope"]))
    .toEqual(["tracker", "statuses", "ENG-1", "ENG-2; echo nope", "--json"]);
});

test("provider batches exclude GitHub notes and foreign tracker prefixes", () => {
  const rows = [{ slug: "eng-1" }, { slug: "gh-2" }, { slug: "legacy-3" }, { slug: "eng-4", issueId: "GH-5" }];
  expect(issueStatusIds(rows, "eng")).toEqual(["ENG-1"]);
  expect(issueStatusIds(rows)).toEqual(["ENG-1", "LEGACY-3"]);
});

describe("status JSON protocol", () => {
  test("preserves arbitrary status vocabulary", () => {
    expect(parseIssueStatuses('{"issues":[{"id":"ENG-1","status":"QA / Awaiting customer"}]}', ["ENG-1"]))
      .toEqual({ "ENG-1": "QA / Awaiting customer" });
  });
  for (const [name, json] of [
    ["malformed JSON", "not JSON"],
    ["wrong shape", '{"ENG-1":"Ready"}'],
    ["missing ID", '{"issues":[]}'],
    ["unexpected ID", '{"issues":[{"id":"ENG-2","status":"Ready"}]}'],
    ["duplicate ID", '{"issues":[{"id":"ENG-1","status":"Ready"},{"id":"ENG-1","status":"Done"}]}'],
    ["empty status", '{"issues":[{"id":"ENG-1","status":" "}]}'],
    ["control character", '{"issues":[{"id":"ENG-1","status":"Ready\\nDone"}]}'],
    ["wrong status type", '{"issues":[{"id":"ENG-1","status":2}]}'],
    ["extra fields", '{"issues":[{"id":"ENG-1","status":"Ready","color":"green"}]}'],
  ]) test(`rejects ${name}`, () => expect(() => parseIssueStatuses(json!, ["ENG-1"])).toThrow("issue status reader"));
});

test("one bounded command reads the batch in main clone cwd", async () => {
  let calls = 0;
  const data = await Effect.runPromise(fetchIssueStatuses(["tracker", "{ids}"], ["ENG-1", "ENG-2"], "/main clone", (args, opts) => {
    calls++;
    expect(args).toEqual(["tracker", "ENG-1", "ENG-2"]);
    expect(opts).toEqual({ cwd: "/main clone", timeoutMs: 30_000 });
    return Effect.succeed({ stdout: JSON.stringify({ issues: [{ id: "ENG-1", status: "Open" }, { id: "ENG-2", status: "Closed" }] }), stderr: "", exitCode: 0 });
  }));
  expect(calls).toBe(1);
  expect(data).toEqual({ "ENG-1": "Open", "ENG-2": "Closed" });
});

test("empty batch starts no reader and a failed reader does not accept partial stdout", async () => {
  expect(await Effect.runPromise(fetchIssueStatuses(["tracker", "{ids}"], [], "/main", () => { throw new Error("must not run"); }))).toEqual({});
  await expect(Effect.runPromise(fetchIssueStatuses(["tracker", "{ids}"], ["ENG-1"], "/main", () =>
    Effect.succeed({ stdout: '{"issues":[{"id":"ENG-1","status":"Open"}]}', stderr: "credentials expired", exitCode: 7 })))).rejects.toThrow("credentials expired");
});
