import { describe, expect, test } from "bun:test";

import { opencodeDisplayTitle } from "./opencode.ts";

describe("opencodeDisplayTitle", () => {
  test("keeps meaningful OpenCode titles", () => {
    expect(
      opencodeDisplayTitle(
        "Fix special session resume behavior",
        "ses_123",
      ),
    ).toBe("Fix special session resume behavior");
  });

  test("drops generated or id-like titles so wt names can win", () => {
    expect(opencodeDisplayTitle("", "ses_123")).toBeNull();
    expect(opencodeDisplayTitle("ses_123", "ses_123")).toBeNull();
    expect(opencodeDisplayTitle("ses_456", "ses_123")).toBeNull();
    expect(
      opencodeDisplayTitle("New session - 2026-04-03T00:17:59.362Z", "ses_123"),
    ).toBeNull();
  });
});
