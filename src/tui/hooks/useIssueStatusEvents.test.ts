import { describe, expect, test } from "bun:test";
import { observeIssueStatuses } from "./useIssueStatusEvents.ts";

describe("issue status attention events", () => {
  test("seeds silently and narrates confirmed transitions once", () => {
    const first = observeIssueStatuses(null, "reader", { "ENG-1": "In Progress" });
    expect(first.lines).toEqual([]);
    const next = observeIssueStatuses(first.observation, "reader", { "ENG-1": "In Review" });
    expect(next.lines).toEqual(["#ENG-1: In Progress → In Review"]);
    expect(observeIssueStatuses(next.observation, "reader", { "ENG-1": "In Review" }).lines).toEqual([]);
  });

  test("new and removed IDs are quiet, including a later reappearance", () => {
    const first = observeIssueStatuses(null, "reader", { "ENG-1": "In Progress" });
    const next = observeIssueStatuses(first.observation, "reader", { "ENG-2": "Done" });
    expect(next.lines).toEqual([]);
    expect(observeIssueStatuses(next.observation, "reader", { "ENG-1": "Done" }).lines).toEqual([]);
  });

  test("inventory changes still compare surviving IDs", () => {
    const first = observeIssueStatuses(null, "reader", { "ENG-1": "In Progress" });
    expect(observeIssueStatuses(first.observation, "reader", {
      "ENG-1": "Done", "ENG-2": "Todo",
    }).lines).toEqual(["#ENG-1: In Progress → Done"]);
  });

  test("an empty inventory clears old identities before they can reappear", () => {
    const first = observeIssueStatuses(null, "reader", { "ENG-1": "In Progress" });
    const empty = observeIssueStatuses(first.observation, "reader", {});
    expect(observeIssueStatuses(empty.observation, "reader", { "ENG-1": "Done" }).lines).toEqual([]);
  });

  test("a provider change seeds instead of comparing unrelated labels", () => {
    const first = observeIssueStatuses(null, "reader-a", { "ENG-1": "In Progress" });
    expect(observeIssueStatuses(first.observation, "reader-b", { "ENG-1": "Doing" }).lines).toEqual([]);
    const loading = observeIssueStatuses(first.observation, "reader-b", undefined);
    expect(observeIssueStatuses(loading.observation, "reader-b", { "ENG-1": "Doing" }).lines).toEqual([]);
  });

  test("missing data preserves a same-provider baseline without announcing success", () => {
    const first = observeIssueStatuses(null, "reader", { "ENG-1": "In Progress" });
    const missing = observeIssueStatuses(first.observation, "reader", undefined);
    expect(missing.lines).toEqual([]);
    expect(observeIssueStatuses(missing.observation, "reader", { "ENG-1": "In Progress" }).lines).toEqual([]);
    expect(observeIssueStatuses(missing.observation, "reader", { "ENG-1": "Done" }).lines).toEqual(["#ENG-1: In Progress → Done"]);
  });
});
