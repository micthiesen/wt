import { Effect } from "effect";
import { idleWorkspaceHost, setWorkspaceTargetLabel, type WorkspaceTarget } from "../../core/workspace.ts";
import { operationErrors } from "../../core/errors.ts";
import { runRemoteWt } from "../../core/remote.ts";
import { navigateWorktreeSession } from "../../tui/sessions/worktree.ts";
const io = operationErrors("workspace host");
export const run = Effect.fn("workspaceHost")(function* (argv: string[]) {
  if (!argv[0]) return yield* idleWorkspaceHost();
  const target = yield* io.sync("decode target", () => JSON.parse(Buffer.from(argv[0]!, "base64url").toString()) as WorkspaceTarget);
  yield* setWorkspaceTargetLabel(target, target.initial);
  const result: Effect.Effect<unknown, Error> = target.remote
    ? runRemoteWt(target.remote, ["_session", target.slug, target.initial, target.harness.harnessId], { interactive: true })
    : navigateWorktreeSession(target);
  const shutdown = Effect.callback<false>((resume) => {
    const stop = () => resume(Effect.succeed(false));
    process.once("SIGHUP", stop);
    process.once("SIGTERM", stop);
    return Effect.sync(() => { process.off("SIGHUP", stop); process.off("SIGTERM", stop); });
  });
  const finished = yield* Effect.raceFirst(
    result.pipe(Effect.catch((error) => Effect.sync(() => console.error(error.message))), Effect.as(true)),
    shutdown,
  );
  if (!finished) return 0;
  return yield* idleWorkspaceHost("Session closed. Select a target in the explorer to reconnect.");
});
