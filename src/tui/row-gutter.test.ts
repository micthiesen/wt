import { expect, test } from "bun:test";

import { NF } from "./icons.ts";
import { releaseMarkerBadge } from "./row-gutter.tsx";

test("release marker changes shape without replacing work-status colour", () => {
  const blocked = { glyph: NF.slash, fg: "#d00" };
  expect(releaseMarkerBadge("base", blocked)).toEqual({ glyph: NF.staging, fg: "#d00" });
  expect(releaseMarkerBadge("production", blocked)).toEqual({ glyph: NF.production, fg: "#d00" });
  expect(releaseMarkerBadge(null, blocked)).toEqual(blocked);
});
