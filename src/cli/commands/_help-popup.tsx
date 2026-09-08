import { useRef, useState } from "react";
import { theme } from "../../tui/theme.ts";
import { createCliRenderer } from "@opentui/core";
import { createRoot, useKeyboard } from "@opentui/react";
import { Effect } from "effect";
import { operationErrors } from "../../core/errors.ts";
import { HelpOverlay } from "../../tui/panels/help.tsx";
import { handleHelpKey } from "../../tui/modal-keys/help.ts";
import type { Modal } from "../../tui/modal-state.ts";
import { emptyEdit } from "../../tui/text-edit.tsx";

const io = operationErrors("help popup");
type Help = Extract<Modal, { kind: "help" }>;
function HelpPopup({ close }: { close: () => void }) {
  const [modal, setState] = useState<Help>({ kind: "help", query: emptyEdit, searching: false });
  const current = useRef<Help>(modal);
  useKeyboard((key) => handleHelpKey(key, current.current, {
    setModal: (update) => {
      const next = typeof update === "function" ? update(current.current) : update;
      if (next?.kind !== "help") { close(); return; }
      current.current = next;
      setState(next);
    },
  }));
  return <HelpOverlay query={modal.query} searching={modal.searching} popup />;
}

/** Only the shared help UI: no fleet queries, startup prompts, or session runtime. */
export const run = Effect.fn("helpPopup")(function* (_argv: string[]) {
  return yield* Effect.acquireUseRelease(
    io.promise("create renderer", () => createCliRenderer({ backgroundColor: theme.bg, exitOnCtrlC: false, autoFocus: false, openConsoleOnError: false })),
    (renderer) => Effect.acquireUseRelease(
      io.sync("create root", () => createRoot(renderer)),
      (root) => Effect.callback<number>((resume) => {
        const close = () => resume(Effect.succeed(0));
        process.once("SIGHUP", close);
        process.once("SIGTERM", close);
        root.render(<HelpPopup close={close} />);
        return Effect.sync(() => { process.off("SIGHUP", close); process.off("SIGTERM", close); });
      }),
      (root) => Effect.sync(() => root.unmount()),
    ),
    (renderer) => Effect.sync(() => renderer.destroy()),
  );
});
