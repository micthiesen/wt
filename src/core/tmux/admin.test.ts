import { describe, expect, test } from "bun:test";

import { classifySessions } from "./admin.ts";
import { HUB_HOME_SESSION } from "./naming.ts";

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
});
