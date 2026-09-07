import { Effect } from "effect";
import { createLogger } from "../../core/logger.ts";
import { workspaceHelpZoom, workspaceSocket } from "../../core/workspace.ts";
import { useEffectFiber } from "./useEffectFiber.ts";

const log = createLogger("[workspace]");

/** One zoom lease for the help overlay, independent of search/filter renders. */
export function useWorkspaceHelp(open: boolean): void {
  useEffectFiber(() => open && workspaceSocket
    ? workspaceHelpZoom.pipe(Effect.catch((error) => Effect.sync(() => {
      log.event.warn(`could not expand help: ${error.message}`);
    })))
    : null, [open]);
}
