import { describe, expect, test } from "bun:test";

import { cwdOsc7 } from "./renderer-handoff.ts";

describe("cwdOsc7", () => {
  test("reports the pane cwd as an encoded file URL", () => {
    expect(cwdOsc7("/tmp/a worktree#1", "devbox.local")).toBe(
      "\x1b]7;file://devbox.local/tmp/a%20worktree%231\x1b\\",
    );
  });
});
