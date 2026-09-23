import { expect, test } from "bun:test";

import type { RemovedWorktree } from "../../core/wtstate.ts";
import { NF } from "../icons.ts";
import { theme } from "../theme.ts";
import { removedPrGlyph, removedStatusGlyph } from "./removed-list.tsx";

const entry: RemovedWorktree = {
  slug: "example",
  branch: "michael/example",
  removedAt: "2026-09-22T00:00:00.000Z",
  prState: "MERGED",
};

test("removed rows distinguish unknown status from saved work status", () => {
  expect(removedStatusGlyph(entry)).toEqual({ glyph: "?", fg: theme.fgDim });
  expect(removedStatusGlyph({
    ...entry,
    work: { state: "ready", at: "2026-09-21T00:00:00.000Z" },
  })).toEqual({ glyph: NF.dot, fg: theme.ok });
  expect(removedStatusGlyph({
    ...entry,
    work: { state: "dropped", at: "2026-09-21T00:00:00.000Z" },
  })).toEqual({ glyph: NF.slash, fg: theme.fgDim });
});

test("merged rows with pending post-merge checks retain the warning status", () => {
  expect(removedStatusGlyph({
    ...entry,
    work: {
      state: "ready",
      at: "2026-09-21T00:00:00.000Z",
      verifyAfterMerge: "check deployed callback",
    },
  }).fg).not.toBe(theme.ok);
  expect(removedPrGlyph(entry)).toEqual({ glyph: NF.prMerged, fg: theme.ok });
});
