import { describe, expect, test } from "bun:test";

import { isTrackerIssueId, issueIdForSlug, repoWebUrl, resolveIssueId } from "./issue-tracker.ts";

test("tracker eligibility excludes notes and scopes configured prefixes", () => {
  expect(isTrackerIssueId("GH-12", null)).toBe(false);
  expect(isTrackerIssueId("ENG-12", "eng")).toBe(true);
  expect(isTrackerIssueId("LEGACY-12", "eng")).toBe(false);
  expect(isTrackerIssueId("LEGACY-12", null)).toBe(true);
  expect(isTrackerIssueId(null, null)).toBe(false);
});

describe("issueIdForSlug", () => {
  test("extracts and uppercases an id at the head of a slug", () => {
    expect(issueIdForSlug("coz-1883-some-fix")).toBe("COZ-1883");
    expect(issueIdForSlug("eng-4959-restack-engine")).toBe("ENG-4959");
  });

  test("matches an id embedded mid-slug (foreign branch layouts)", () => {
    expect(issueIdForSlug("worktree-david+eng-4959-thing")).toBe("ENG-4959");
  });

  test("bare id with no trailing slug", () => {
    expect(issueIdForSlug("coz-1883")).toBe("COZ-1883");
  });

  test("null for slugs without an id", () => {
    expect(issueIdForSlug("quick-spike")).toBeNull();
    expect(issueIdForSlug("")).toBeNull();
  });

  test("gh-prefixed ids parse like any other", () => {
    expect(issueIdForSlug("gh-970-fix-typo")).toBe("GH-970");
  });
});

describe("repoWebUrl", () => {
  test("scp-style ssh remote", () => {
    expect(repoWebUrl("git@github.com:acme/webapp.git")).toBe(
      "https://github.com/acme/webapp",
    );
  });

  test("https remote, with and without .git", () => {
    expect(repoWebUrl("https://github.com/o/r.git")).toBe("https://github.com/o/r");
    expect(repoWebUrl("https://github.com/o/r")).toBe("https://github.com/o/r");
  });

  test("ssh:// remote", () => {
    expect(repoWebUrl("ssh://git@github.com/o/r.git")).toBe("https://github.com/o/r");
  });

  test("null for bare ssh-config host aliases (no real hostname)", () => {
    expect(repoWebUrl("github-personal:micthiesen/wt.git")).toBeNull();
  });
});

describe("resolveIssueId — three states", () => {
  // The distinction only shows on a slug that CARRIES an id, which is
  // exactly the population `--no-id` exists for: before the empty
  // string meant something, clearing the override on `coz-2101-…` fell
  // straight back to COZ-2101 and there was no way to say otherwise.
  test("absent override falls back to the slug", () => {
    expect(resolveIssueId("coz-2101-connector", undefined)).toBe("COZ-2101");
    expect(resolveIssueId("coz-2101-connector", null)).toBe("COZ-2101");
  });

  test("a value overrides the slug", () => {
    expect(resolveIssueId("coz-2101-connector", "COZ-9")).toBe("COZ-9");
  });

  test("the empty string is an asserted none, and beats the slug", () => {
    expect(resolveIssueId("coz-2101-connector", "")).toBeNull();
  });

  test("an asserted none on a slug with no id is still none", () => {
    expect(resolveIssueId("quick-spike", "")).toBeNull();
    expect(resolveIssueId("quick-spike", undefined)).toBeNull();
  });

  test("a whitespace-only override is a none, not a fallback", () => {
    // It reaches the store trimmed, but a hand-edited state.json can
    // carry one and "  " must not silently resurrect the slug's id.
    expect(resolveIssueId("coz-2101-connector", "   ")).toBeNull();
  });
});
