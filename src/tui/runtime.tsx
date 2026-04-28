import { createCliRenderer } from "@opentui/core";
import { createRoot } from "@opentui/react";
import { QueryClientProvider } from "@tanstack/react-query";

import { createWtQueryClient } from "../state/index.ts";

import { App, type TuiExit } from "./app.tsx";
import { attachFetchLogs } from "./fetch-log.ts";

export async function runTui(): Promise<TuiExit> {
  const wtClient = createWtQueryClient();
  const detachFetchLogs = attachFetchLogs(wtClient.client);
  // Wait briefly for the SQLite cache to hydrate so the first paint
  // shows stale data instead of empty. If hydration takes longer than
  // the budget we render anyway and it'll swap in when ready.
  await Promise.race([
    wtClient.restored,
    new Promise<void>((r) => setTimeout(r, 150)),
  ]);

  const renderer = await createCliRenderer({
    exitOnCtrlC: false,
    targetFps: 60,
  });
  const root = createRoot(renderer);
  return new Promise<TuiExit>((resolve) => {
    root.render(
      <QueryClientProvider client={wtClient.client}>
        <App onExit={resolve} />
      </QueryClientProvider>,
    );
  }).finally(() => {
    // Tear down listeners, timers, and the SQLite handle so the
    // process can exit cleanly. Each step may throw if an earlier
    // one already disposed state — we swallow so all three run.
    try {
      root.unmount();
    } catch (err) {
      void err;
    }
    try {
      renderer.destroy();
    } catch (err) {
      void err;
    }
    detachFetchLogs();
    wtClient.shutdown();
  });
}
