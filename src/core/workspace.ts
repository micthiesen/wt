/** Experimental presentation workspace. Persistent sessions belong to tmux.ts. */
import { mkdtempSync, readFileSync, writeFileSync, rmSync } from "node:fs";
import { tmpdir, homedir } from "node:os";
import { join, resolve } from "node:path";
import { Effect, Semaphore } from "effect";
import { operationErrors } from "./errors.ts";
import { run, terminateSubprocess } from "./proc.ts";
import { TERMINAL_PREAMBLE } from "./tmux/config.ts";
import type { HarnessRoute, WorktreeSessionTarget } from "../tui/sessions/worktree.ts";
import type { RemoteConfig } from "./config.ts";

const io = operationErrors("workspace");
const gate = Semaphore.makeUnsafe(1);
const entry = resolve(import.meta.dir, "../main.ts");
export const workspaceSocket = process.env.WT_WORKSPACE_SOCKET;
export type WorkspaceTarget = {
  slug: string; cwd: string; initial: WorktreeSessionTarget;
  diffBase: string; harness: HarnessRoute; switchable?: boolean;
  remote?: RemoteConfig;
};
const quote = (s: string) => `'${s.replaceAll("'", "'\\''")}'`;
const command = (argv: string[]) => argv.map(quote).join(" ");
export const targetIdentity = (t: WorkspaceTarget): string => JSON.stringify({
  host: t.remote?.host ?? "local", slug: t.slug, cwd: t.cwd, kind: t.initial,
  harness: t.harness.harnessId, name: t.harness.managedName ?? null,
  resume: t.harness.resumeSessionId ?? null, base: t.diffBase, switchable: t.switchable !== false,
});
function decodedTarget(value: string): WorkspaceTarget | null {
  try { return JSON.parse(Buffer.from(value, "base64url").toString()) as WorkspaceTarget; }
  catch { return null; }
}
export function sameDisplayedTarget(requested: WorkspaceTarget, displayed: WorkspaceTarget): boolean {
  if (requested.harness.freshSlot) return false;
  const normalized = requested.harness.resumeSessionId ? displayed : {
    ...displayed, harness: { ...displayed.harness, resumeSessionId: null },
  };
  return targetIdentity(requested) === targetIdentity(normalized);
}

/** Only copy prefix and simple pane-navigation bindings, never source user config. */
export function navigationBindings(text: string): string[] {
  return text.split("\n").map((line) => line.trim()).filter((line) =>
    /^(?:set|set-option) -g prefix [A-Za-z0-9-]+$/.test(line) ||
    /^bind(?:-key)? (?:-n )?[A-Za-z0-9-]+ select-pane -[LDUR]$/.test(line),
  );
}
export function workspaceConfig(navigation: readonly string[]): string {
  return `${TERMINAL_PREAMBLE}\n${navigation.join("\n")}\nset -g pane-border-status top
set -g pane-border-format '#{pane_title}'
bind-key -n F9 select-pane -t %0 \\; resize-pane -Z -t %0
${(["shell", "diff", "harness"] as const).map((kind, i) =>
  `bind-key -n F${10 + i} if-shell -F '#{&&:#{!=:#{pane_id},%0},#{||:#{==:#{@wt-active},${kind}},#{==:#{@wt-slot},1}}}' 'select-pane -t %0' 'send-keys F${10 + i}'`,
).join("\n")}\n`;
}
const tmux = Effect.fn("workspaceTmux")(function* (socket: string, args: string[]) {
  const result = yield* run(["tmux", "-L", socket, ...args]);
  if (result.exitCode !== 0) return yield* io.wrap("tmux")(new Error(result.stderr.trim()));
  return result.stdout.trim();
});

function inherited(argv: string[], env: Record<string, string | undefined> = process.env) {
  return Effect.acquireUseRelease(
    io.sync("spawn terminal client", () => Bun.spawn(argv, { stdin: "inherit", stdout: "inherit", stderr: "inherit", env })),
    (proc) => io.promise("wait terminal client", () => proc.exited),
    (proc) => terminateSubprocess(proc),
  );
}

export const launchWorkspace = Effect.fn("launchWorkspace")(function* () {
  return yield* Effect.acquireUseRelease(
    io.sync("allocate workspace", () => {
      const dir = mkdtempSync(join(tmpdir(), "wt-workspace-"));
      const socket = `wt-view-${process.pid}-${Date.now()}`;
      let navigation: string[] = [];
      for (const path of [join(homedir(), ".tmux.conf"), join(homedir(), ".config/tmux/tmux.conf")]) {
        try { navigation.push(...navigationBindings(readFileSync(path, "utf8"))); } catch {}
      }
      const conf = join(dir, "tmux.conf");
      writeFileSync(conf, workspaceConfig(navigation));
      return { dir, socket, conf };
    }),
    ({ socket, conf }) => Effect.gen(function* () {
      const env = ["env", "-u", "BUN_INSPECT", `WT_WORKSPACE_SOCKET=${socket}`, "WT_WORKSPACE=off"];
      // Use a holding pane until both stable IDs exist, then start the explorer.
      const explorer = yield* tmux(socket, ["-f", conf, "new-session", "-d", "-P", "-F", "#{pane_id}", "-s", "workspace", "-x", String(process.stdout.columns), "-y", String(process.stdout.rows), command([...env, "bun", entry, "_workspace-host"])]);
      yield* tmux(socket, ["set-option", "-t", "workspace", "@wt-explorer", explorer]);
      yield* tmux(socket, ["set-option", "-t", "workspace", "destroy-unattached", "off"]);
      const content = yield* tmux(socket, ["split-window", "-h", "-P", "-F", "#{pane_id}", "-t", explorer, "-l", String(Math.max(40, process.stdout.columns - 53)), command([...env, "bun", entry, "_workspace-host"])]);
      yield* tmux(socket, ["set-option", "-t", "workspace", "@wt-content", content]);
      yield* tmux(socket, ["respawn-pane", "-k", "-t", explorer, command([...env, "WT_UPDATE=off", "WT_SKILLS=off", "bun", entry])]);
      const explorerPid = yield* tmux(socket, ["display-message", "-p", "-t", explorer, "#{pane_pid}"]);
      yield* tmux(socket, ["set-option", "-p", "-t", explorer, "@wt-owner", explorerPid]);
      yield* tmux(socket, ["select-pane", "-t", explorer, "-T", "wt explorer · F9 dashboard"]);
      yield* tmux(socket, ["select-pane", "-t", explorer]);
      const explorerExited = Effect.gen(function* () {
        for (;;) {
          const state = yield* run(["tmux", "-L", socket, "display-message", "-p", "-t", explorer, "#{pane_pid}"]);
          if (state.exitCode !== 0 || state.stdout.trim() !== explorerPid) return 0;
          yield* Effect.sleep("250 millis");
        }
      });
      return yield* Effect.raceFirst(
        inherited(["tmux", "-L", socket, "attach-session", "-t", "workspace"], { ...process.env, TMUX: undefined }),
        explorerExited,
      );
    }),
    ({ dir, socket }) => Effect.gen(function* () {
      // Remove only the two owned presentation panes; user-added panes survive.
      const panes = yield* run(["tmux", "-L", socket, "list-panes", "-t", "workspace", "-F", "#{pane_id}\t#{pane_current_command}\t#{pane_pid}\t#{@wt-owner}"]).pipe(Effect.option);
      const content = yield* run(["tmux", "-L", socket, "show-option", "-v", "-t", "workspace", "@wt-content"]).pipe(Effect.option);
      if (panes._tag === "Some") {
        for (const line of panes.value.stdout.trim().split("\n")) {
          const [id, cmd, pid, owner] = line.split("\t");
          if (cmd === "bun" && pid === owner && (id === "%0" || (content._tag === "Some" && id === content.value.stdout.trim()))) {
            yield* run(["tmux", "-L", socket, "kill-pane", "-t", id!]).pipe(Effect.ignore);
          }
        }
      }
      yield* io.sync("remove workspace config", () => rmSync(dir, { recursive: true, force: true })).pipe(Effect.ignore);
    }),
  );
});

let newestRequest = 0;
export const showWorkspaceTarget = Effect.fn("showWorkspaceTarget")(function* (target: WorkspaceTarget) {
  const socket = workspaceSocket;
  if (!socket) return;
  const request = ++newestRequest;
  yield* Effect.gen(function* () {
    if (request !== newestRequest) return;
    // Tags alone are not ownership: this pane must still be our explorer process.
    const explorer = process.env.TMUX_PANE!;
    const owner = yield* tmux(socket, ["display-message", "-p", "-t", explorer, "#{pane_pid}"]);
    if (owner !== String(process.pid)) return yield* io.wrap("open target")(new Error("workspace explorer ownership changed"));
    const identity = Buffer.from(JSON.stringify(target)).toString("base64url");
    const contentId = yield* tmux(socket, ["show-option", "-v", "-t", "workspace", "@wt-content"]);
    const panes = yield* tmux(socket, ["list-panes", "-t", "workspace", "-F", "#{pane_id}\t#{pane_pid}\t#{pane_current_command}\t#{@wt-target}\t#{@wt-owner}"]);
    const content = panes.split("\n").find((line) => line.startsWith(`${contentId}\t`));
    if (content && (content.split("\t")[2] !== "bun" || content.split("\t")[1] !== content.split("\t")[4])) return yield* io.wrap("open target")(new Error("content pane is no longer owned by wt"));
    let activeContent = contentId;
    const displayed = decodedTarget(content?.split("\t")[3] ?? "");
    const matches = displayed !== null && sameDisplayedTarget(target, displayed);
    if (request !== newestRequest) return;
    if (!content || !matches) {
      const cmd = command(["env", "-u", "BUN_INSPECT", `WT_WORKSPACE_SOCKET=${socket}`, "WT_WORKSPACE=off", "bun", entry, "_workspace-host", identity]);
      if (content) yield* tmux(socket, ["respawn-pane", "-k", "-t", contentId, cmd]);
      else {
        activeContent = yield* tmux(socket, ["split-window", "-h", "-P", "-F", "#{pane_id}", "-t", explorer, "-p", "65", cmd]);
        yield* tmux(socket, ["set-option", "-t", "workspace", "@wt-content", activeContent]);
      }
      const contentPid = yield* tmux(socket, ["display-message", "-p", "-t", activeContent, "#{pane_pid}"]);
      yield* tmux(socket, ["set-option", "-p", "-t", activeContent, "@wt-owner", contentPid]);
      yield* tmux(socket, ["set-option", "-p", "-t", activeContent, "@wt-target", identity]);
    }
    if (request !== newestRequest) return;
    const zoomed = yield* tmux(socket, ["display-message", "-p", "-t", "workspace:0", "#{window_zoomed_flag}"]);
    if (zoomed === "1") yield* tmux(socket, ["resize-pane", "-Z", "-t", explorer]);
    yield* tmux(socket, ["select-pane", "-t", activeContent]);
  }).pipe(gate.withPermit);
});

export const setWorkspaceTargetLabel = Effect.fn("setWorkspaceTargetLabel")(function* (target: WorkspaceTarget, kind: WorktreeSessionTarget) {
  if (!workspaceSocket) return;
  const owner = yield* tmux(workspaceSocket, ["display-message", "-p", "-t", process.env.TMUX_PANE!, "#{pane_pid}"]);
  if (owner !== String(process.pid)) return;
  yield* tmux(workspaceSocket, ["set-option", "-p", "-t", process.env.TMUX_PANE!, "@wt-owner", String(process.pid)]);
  yield* tmux(workspaceSocket, ["set-option", "-p", "-t", process.env.TMUX_PANE!, "@wt-target",
    Buffer.from(JSON.stringify({ ...target, initial: kind })).toString("base64url")]);
  yield* tmux(workspaceSocket, ["set-option", "-p", "-t", process.env.TMUX_PANE!, "@wt-active", kind]);
  yield* tmux(workspaceSocket, ["set-option", "-p", "-t", process.env.TMUX_PANE!, "@wt-slot", target.switchable === false ? "1" : "0"]);
  yield* tmux(workspaceSocket, ["select-pane", "-t", process.env.TMUX_PANE!, "-T", `${target.remote?.label ?? "local"} · ${target.slug} · ${kind === "harness" ? target.harness.harnessId : kind}`]);
});

export const idleWorkspaceHost = Effect.fn("idleWorkspaceHost")(function* (message = "Choose a worktree, then F10 shell · F11 diff · F12 agent") {
  if (workspaceSocket && process.env.TMUX_PANE) {
    yield* tmux(workspaceSocket, ["set-option", "-p", "-t", process.env.TMUX_PANE, "@wt-owner", String(process.pid)]);
    yield* tmux(workspaceSocket, ["set-option", "-p", "-t", process.env.TMUX_PANE, "@wt-target", ""]);
    yield* tmux(workspaceSocket, ["select-pane", "-t", process.env.TMUX_PANE, "-T", "wt · idle"]);
  }
  yield* Effect.sync(() => { process.stdin.setRawMode?.(true); process.stdin.resume(); });
  yield* Effect.sync(() => console.log(`\n  ${message}\n\n  F9: full dashboard`));
  return yield* Effect.never;
});

/** Help temporarily owns explorer zoom; preserve a dashboard already zoomed by F9. */
export const workspaceHelpZoom = Effect.acquireUseRelease(
  Effect.gen(function* () {
    const socket = workspaceSocket;
    const pane = process.env.TMUX_PANE;
    if (!socket || !pane) return null;
    const state = yield* tmux(socket, ["display-message", "-p", "-t", pane, "#{pane_pid} #{window_zoomed_flag}"]);
    if (state !== `${process.pid} 0`) return null;
    yield* tmux(socket, ["resize-pane", "-Z", "-t", pane]);
    return { socket, pane };
  }),
  () => Effect.never,
  (owned) => Effect.gen(function* () {
    if (!owned) return;
    const state = yield* tmux(owned.socket, ["display-message", "-p", "-t", owned.pane, "#{pane_pid} #{window_zoomed_flag}"]);
    if (state === `${process.pid} 1`) {
      yield* tmux(owned.socket, ["resize-pane", "-Z", "-t", owned.pane]);
    }
  }).pipe(Effect.ignore),
).pipe(gate.withPermit);
