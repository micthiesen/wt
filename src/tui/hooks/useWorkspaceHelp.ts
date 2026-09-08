import type { Dispatch, SetStateAction } from "react";
import { Effect } from "effect";
import { createLogger } from "../../core/logger.ts";
import { workspaceHelpPopup, workspaceSocket } from "../../core/workspace.ts";
import type { Modal } from "../modal-state.ts";
import { useEffectFiber } from "./useEffectFiber.ts";

const log = createLogger("[workspace]");

export function useWorkspaceHelp(open: boolean, setModal: Dispatch<SetStateAction<Modal | null>>): void {
  useEffectFiber(() => open && workspaceSocket
    ? workspaceHelpPopup.pipe(
      Effect.catch((error) => Effect.sync(() => {
        log.event.warn(`could not open help: ${error.message}`);
      })),
      Effect.ensuring(Effect.sync(() => setModal((current) => current?.kind === "help" ? null : current))),
    )
    : null, [open, setModal]);
}
