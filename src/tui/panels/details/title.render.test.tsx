import { expect, test } from "bun:test";
import { act } from "react";
import { useTerminalDimensions } from "@opentui/react";
import { testRender } from "@opentui/react/test-utils";

import { DetailTitleLine } from "./title.tsx";

const title = "iOS uploads move to R2 with 日本語 and 👩🏽‍💻 previews intact";

function Fixture() {
  const { width } = useTerminalDimensions();
  // Deliberately narrower than the terminal: native wrapping to terminal
  // width used to hide the tail behind the pane edge.
  const paneWidth = Math.floor(width / 2);
  return (
    <box width={paneWidth} border flexDirection="column">
      <DetailTitleLine title={title} source="manual" contentWidth={paneWidth - 2} />
      <text>Next row</text>
    </box>
  );
}

test("details retain the full title, casing, and source across split-pane resizes", async () => {
  const setup = await testRender(<Fixture />, { width: 110, height: 20 });
  try {
    for (const width of [110, 60, 160, 110]) {
      act(() => setup.resize(width, 20));
      await setup.flush();
      const frame = setup.captureCharFrame();
      const body = frame.split("\n").filter((line) => line.startsWith("│"))
        .map((line) => line.split("│")[1]!.trim()).join(" ");
      expect(body).toContain(`${title} (manual) Next row`);
      expect(frame).not.toContain("...");
      expect(frame).not.toContain("�");
    }
  } finally { act(() => setup.renderer.destroy()); }
});
