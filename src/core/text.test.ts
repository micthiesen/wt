import { expect, test } from "bun:test";
import { formatDuration } from "./text.ts";

test("duration logs round sub-millisecond clocks and use bounded precision", () => {
  expect(formatDuration(799.797119140625)).toBe("800ms");
  expect(formatDuration(0.01)).toBe("0ms");
  expect(formatDuration(999.9)).toBe("1s");
  expect(formatDuration(1234.567)).toBe("1.2s");
  expect(formatDuration(12000)).toBe("12s");
  expect(formatDuration(79999.797)).toBe("1m20s");
  expect(formatDuration(NaN)).toBe("unknown duration");
});
