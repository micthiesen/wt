import { describe, expect, test } from "bun:test";

import { classifySessions, orphanedSessions } from "./admin.ts";
import { HUB_HOME_SESSION } from "./naming.ts";
import { parseRemoteWrapper, remoteWrapperName } from "./remote-wrapper.ts";

describe("classifySessions", () => {
  test("partitions raw session names by kind", () => {
    const result = classifySessions([
      "eng-1234-foo",
      "eng-1234-foo~scratch",
      "eng-5678-bar-codex",
      "eng-5678-bar-opencode",
      "eng-9999-baz-diff",
      "eng-9999-baz-shell",
      "eng-1111-qux-action",
    ]);
    expect(result.claude).toEqual([
      { slug: "eng-1234-foo", name: null },
      { slug: "eng-1234-foo", name: "scratch" },
    ]);
    expect(result.claudeSlugs).toEqual(new Set(["eng-1234-foo"]));
    expect(result.codex).toEqual(new Set(["eng-5678-bar"]));
    expect(result.opencode).toEqual(new Set(["eng-5678-bar"]));
    expect(result.diff).toEqual(new Set(["eng-9999-baz"]));
    expect(result.shell).toEqual(new Set(["eng-9999-baz"]));
    expect(result.action).toEqual(new Set(["eng-1111-qux"]));
    expect(result.remote).toEqual([]);
  });

  test("excludes the reserved hub-home session from every classified set", () => {
    const result = classifySessions([HUB_HOME_SESSION, "eng-1234-foo"]);
    expect(result.claude).toEqual([{ slug: "eng-1234-foo", name: null }]);
    expect(result.claudeSlugs.has(HUB_HOME_SESSION)).toBe(false);
    expect(result.claudeSlugs).toEqual(new Set(["eng-1234-foo"]));
    // Sanity: it doesn't slip in through any other set either.
    expect(result.codex.size).toBe(0);
    expect(result.opencode.size).toBe(0);
    expect(result.diff.size).toBe(0);
    expect(result.shell.size).toBe(0);
    expect(result.action.size).toBe(0);
  });

  test("hub-home session alone classifies to nothing", () => {
    const result = classifySessions([HUB_HOME_SESSION]);
    expect(result.claude).toEqual([]);
    expect(result.claudeSlugs.size).toBe(0);
  });

  test("remote wrapper sessions classify into `remote`, never the local kinds", () => {
    const result = classifySessions([
      "wt-remote~eng-1234-foo~harness",
      "wt-remote~eng-1234-foo~diff",
      "wt-remote~eng-1234-foo~shell",
      "eng-1234-foo",
    ]);
    expect(result.remote).toEqual([
      { slug: "eng-1234-foo", target: "harness", name: "wt-remote~eng-1234-foo~harness" },
      { slug: "eng-1234-foo", target: "diff", name: "wt-remote~eng-1234-foo~diff" },
      { slug: "eng-1234-foo", target: "shell", name: "wt-remote~eng-1234-foo~shell" },
    ]);
    // A wrapper must NOT leak a phantom local session of any kind —
    // the exact bug the prefix carve-out exists to prevent.
    expect(result.diff.size).toBe(0);
    expect(result.shell.size).toBe(0);
    // A wrapper name must not read as a named-claude session either.
    expect(result.claude).toEqual([{ slug: "eng-1234-foo", name: null }]);
  });
});

describe("orphanedSessions", () => {
  test("reaps dead-slug sessions but never the reserved names", () => {
    const live = new Set(["eng-1234-foo"]);
    const orphans = orphanedSessions(
      [
        "eng-1234-foo", // live slug — kept
        "eng-1234-foo-diff", // live slug, diff kind — kept
        "eng-9999-gone", // dead slug — reaped
        "eng-9999-gone-shell", // dead slug, shell kind — reaped
        HUB_HOME_SESSION, // reserved — never reaped
        remoteWrapperName("remote-only-slug", "harness"), // SSH-bound — never reaped
      ],
      live,
    );
    expect(orphans).toEqual(["eng-9999-gone", "eng-9999-gone-shell"]);
  });
});

describe("remoteWrapperName / parseRemoteWrapper", () => {
  test("round-trips every target", () => {
    for (const target of ["harness", "diff", "shell"] as const) {
      const name = remoteWrapperName("eng-42-thing", target);
      expect(parseRemoteWrapper(name)).toEqual({
        slug: "eng-42-thing",
        target,
        name,
      });
    }
  });

  test("remote slugs ending in a local kind suffix stay unambiguous", () => {
    // Remote slugs are unvalidated remote strings — a slug like
    // `eng-1-fix-shell` must not collide with `eng-1-fix`'s shell
    // wrapper (the suffix-based encoding this replaced had exactly
    // that collision).
    const a = remoteWrapperName("eng-1-fix-shell", "harness");
    const b = remoteWrapperName("eng-1-fix", "shell");
    expect(a).not.toBe(b);
    expect(parseRemoteWrapper(a)).toEqual({
      slug: "eng-1-fix-shell",
      target: "harness",
      name: a,
    });
    expect(parseRemoteWrapper(b)).toEqual({
      slug: "eng-1-fix",
      target: "shell",
      name: b,
    });
  });

  test("non-wrapper and malformed names parse to null", () => {
    expect(parseRemoteWrapper("eng-1234-foo")).toBeNull();
    expect(parseRemoteWrapper("eng-1234-foo-diff")).toBeNull();
    expect(parseRemoteWrapper(HUB_HOME_SESSION)).toBeNull();
    // Prefixed but no ~<target> segment, or an unknown target: ignored
    // rather than misattributed.
    expect(parseRemoteWrapper("wt-remote~eng-1234-foo")).toBeNull();
    expect(parseRemoteWrapper("wt-remote~eng-1234-foo~bogus")).toBeNull();
  });
});
