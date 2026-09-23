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

test("removed rows show outcome rather than saved agent status", () => {
  expect(removedStatusGlyph(entry)).toEqual({ glyph: NF.merge, fg: theme.ok });
  expect(removedStatusGlyph({
    ...entry,
    work: { state: "todo", at: "2026-09-21T00:00:00.000Z" },
  })).toEqual({ glyph: NF.merge, fg: theme.ok });
  expect(removedStatusGlyph({
    ...entry,
    prState: "CLOSED",
  })).toEqual({ glyph: NF.prClosed, fg: theme.err });
  expect(removedStatusGlyph({ ...entry, prState: undefined, gitState: "merged" })).toEqual({ glyph: NF.merge, fg: theme.ok });
  expect(removedStatusGlyph({ ...entry, prState: undefined, gitState: "gone" })).toEqual({ glyph: NF.slash, fg: theme.warn });
  expect(removedStatusGlyph({
    ...entry,
    prState: undefined,
    work: { state: "dropped", at: "2026-09-21T00:00:00.000Z" },
  })).toEqual({ glyph: NF.slash, fg: theme.fgDim });
  expect(removedStatusGlyph({ ...entry, prState: undefined })).toEqual({ glyph: NF.trash, fg: theme.fgDim });
});

test("verification obligation remains in details, not the outcome column", () => {
  expect(removedStatusGlyph({
    ...entry,
    work: {
      state: "ready",
      at: "2026-09-21T00:00:00.000Z",
      verifyAfterMerge: "check deployed callback",
    },
  })).toEqual({ glyph: NF.merge, fg: theme.ok });
  expect(removedPrGlyph(entry)).toEqual({ glyph: NF.prMerged, fg: theme.ok });
});
