import { describe, expect, test } from "bun:test";

import { isRunningInWezTerm, wezTermCliPath } from "./wezterm.ts";

describe("isRunningInWezTerm", () => {
  test("detects a WezTerm pane", () => {
    expect(isRunningInWezTerm({ WEZTERM_PANE: "7" })).toBe(true);
  });

  test("does not rely on TERM_PROGRAM", () => {
    expect(isRunningInWezTerm({ TERM_PROGRAM: "WezTerm" })).toBe(false);
  });
});

describe("wezTermCliPath", () => {
  test("prefers the configured path", () => {
    expect(wezTermCliPath("/custom/wezterm", () => "/opt/bin/wezterm")).toBe(
      "/custom/wezterm",
    );
  });

  test("falls back to wezterm from PATH", () => {
    expect(wezTermCliPath(null, () => "/opt/bin/wezterm")).toBe("/opt/bin/wezterm");
  });

  test("returns null when no configured or PATH executable exists", () => {
    expect(wezTermCliPath(null, () => null)).toBeNull();
  });
});
