import { useEffect, useRef, type Dispatch, type SetStateAction } from "react";
import { createServer, type Socket } from "node:net";
import { mkdtempSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { KeyEvent } from "@opentui/core";
import { Effect } from "effect";
import { operationErrors, type OperationError } from "../../core/errors.ts";
import { createLogger } from "../../core/logger.ts";
import { showWorkspacePopup, workspaceSocket } from "../../core/workspace.ts";
import { isActionPopupModal, type ActionPopupSnapshot } from "../action-popup.tsx";
import type { Modal } from "../modal-state.ts";
import { useEffectFiber } from "./useEffectFiber.ts";
const io = operationErrors("action popup");
const log = createLogger("[workspace]");

export function useWorkspaceActions(snapshot: ActionPopupSnapshot | null,
  onKey: (key: KeyEvent) => void, onPaste: (text: string) => void,
  setModal: Dispatch<SetStateAction<Modal | null>>): void {
  const latest = useRef({ snapshot, onKey, onPaste });
  latest.current = { snapshot, onKey, onPaste };
  const publish = useRef<(() => void) | null>(null);
  useEffect(() => { publish.current?.(); }, [snapshot]);
  const open = workspaceSocket !== undefined && snapshot !== null;
  useEffectFiber(() => !open ? null : Effect.acquireUseRelease(
    io.sync("create popup bridge", () => {
      const dir = mkdtempSync(join(tmpdir(), "wt-action-popup-"));
      const path = join(dir, "ui.sock");
      const peers = new Set<Socket>();
      const server = createServer((socket) => {
        peers.add(socket);
        let buffer = "";
        socket.setEncoding("utf8");
        socket.on("error", () => socket.destroy());
        socket.on("close", () => peers.delete(socket));
        socket.on("data", (data) => {
          buffer += data;
          if (buffer.length > 1024 * 1024) { socket.destroy(); return; }
          let end: number;
          while ((end = buffer.indexOf("\n")) >= 0) {
            const line = buffer.slice(0, end); buffer = buffer.slice(end + 1);
            try {
              const event = JSON.parse(line);
              if (event.kind === "key") latest.current.onKey(new KeyEvent(event.key));
              else if (event.kind === "paste" && typeof event.text === "string") latest.current.onPaste(event.text);
            } catch (error) { log.warn("invalid popup input", { error: String(error) }); }
          }
        });
        socket.write(JSON.stringify(latest.current.snapshot) + "\n");
      });
      let previous = "";
      publish.current = () => {
        const data = JSON.stringify(latest.current.snapshot) + "\n";
        if (data === previous) return;
        previous = data;
        for (const socket of peers) socket.write(data);
      };
      return { dir, path, server, peers };
    }),
    ({ path, server }) => Effect.callback<void, OperationError>((resume) => {
      const fail = (error: Error) => resume(Effect.fail(io.wrap("listen for popup")(error)));
      server.once("error", fail);
      server.listen(path, () => resume(Effect.void));
      return Effect.sync(() => { server.off("error", fail); });
    }).pipe(Effect.andThen(showWorkspacePopup(["_action-popup", path], 0.65))),
    ({ dir, server, peers }) => io.sync("close popup bridge", () => {
      publish.current = null;
      for (const socket of peers) socket.destroy();
      server.close();
      rmSync(dir, { recursive: true, force: true });
      setModal((current) => isActionPopupModal(current) ? null : current);
    }).pipe(Effect.ignore),
  ).pipe(Effect.catch((error) => Effect.sync(() => {
    log.event.warn(`could not open actions: ${error.message}`);
  }))), [open, setModal]);
}
