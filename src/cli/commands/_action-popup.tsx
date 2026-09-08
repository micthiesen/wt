import { useSyncExternalStore } from "react";
import { createConnection } from "node:net";
import { createCliRenderer } from "@opentui/core";
import { createRoot, useKeyboard } from "@opentui/react";
import { Effect } from "effect";
import { operationErrors } from "../../core/errors.ts";
import { ActionPopupView, type ActionPopupSnapshot } from "../../tui/action-popup.tsx";
import { usePaste } from "../../tui/hooks/usePaste.ts";
const io = operationErrors("action popup renderer");
type SnapshotStore = {
  getSnapshot: () => ActionPopupSnapshot | null;
  subscribe: (listener: () => void) => () => void;
};
function Popup({ store, send }: { store: SnapshotStore; send: (event: unknown) => void }) {
  const snapshot = useSyncExternalStore(store.subscribe, store.getSnapshot);
  useKeyboard((key) => send({ kind: "key", key }));
  usePaste((text) => send({ kind: "paste", text }));
  return snapshot ? <ActionPopupView {...snapshot} /> : null;
}
export const run = Effect.fn("actionPopup")(function* (argv: string[]) {
  if (argv.length !== 1) return 2;
  return yield* Effect.acquireUseRelease(
    io.promise("create renderer", () => createCliRenderer({ exitOnCtrlC: false, autoFocus: false, openConsoleOnError: false })),
    (renderer) => Effect.acquireUseRelease(
      io.sync("create root", () => createRoot(renderer)),
      (root) => Effect.callback<number>((resume) => {
        const socket = createConnection(argv[0]!);
        const close = () => resume(Effect.succeed(0));
        let buffer = "";
        let snapshot: ActionPopupSnapshot | null = null;
        const listeners = new Set<() => void>();
        const store: SnapshotStore = {
          getSnapshot: () => snapshot,
          subscribe: (listener) => { listeners.add(listener); return () => { listeners.delete(listener); }; },
        };
        // OpenTUI creates a new reconciler container on each root.render call.
        // Mount once so snapshot updates cannot accumulate keyboard listeners.
        root.render(<Popup store={store} send={(event) => socket.write(JSON.stringify(event) + "\n")} />);
        socket.setEncoding("utf8");
        socket.on("error", close);
        socket.on("close", close);
        socket.on("data", (data) => {
          buffer += data;
          let end: number;
          while ((end = buffer.indexOf("\n")) >= 0) {
            const line = buffer.slice(0, end); buffer = buffer.slice(end + 1);
            try {
              snapshot = JSON.parse(line) as ActionPopupSnapshot | null;
              if (!snapshot) { close(); return; }
              for (const listener of listeners) listener();
            } catch { close(); }
          }
        });
        process.once("SIGHUP", close); process.once("SIGTERM", close);
        return Effect.sync(() => {
          process.off("SIGHUP", close); process.off("SIGTERM", close); socket.destroy();
        });
      }),
      (root) => Effect.sync(() => root.unmount()),
    ),
    (renderer) => Effect.sync(() => renderer.destroy()),
  );
});
