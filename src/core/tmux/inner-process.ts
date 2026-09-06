import { browserSessionName } from "../browser.ts";
import { delimiter, join } from "node:path";
import { Effect } from "effect";
import {
  clearInspectorSocket,
  ensureInspectorDir,
  ensureInspectShims,
  inspectorSocketPath,
  pathWithShims,
} from "../harness/claude/inject.ts";
import { createLogger } from "../logger.ts";
import { harnessIdForKind, type SessionKind } from "./naming.ts";
import { probeSessionNames } from "./process.ts";

const log = createLogger("[tmux]");
const SOURCE_WT_BIN_DIR = join(import.meta.dir, "..", "..", "..", "bin");

/** The installation that launched this wt, with source checkout fallback. */
function launcherBinDir(): string {
  return process.env.WT_LAUNCHER_DIR || SOURCE_WT_BIN_DIR;
}

type InnerSessionKind = Exclude<SessionKind, "action" | "dev">;

/** Whether a session preserves short-lived startup errors after its pane exits. */
export function capturesInnerStderr(kind: InnerSessionKind): boolean {
  return kind !== "shell";
}

/**
 * Environment a Claude session must NOT inherit from whoever started
 * wt.
 *
 * wt is usually run BY an agent, so wt's environment is a Claude
 * session's environment, and every one of these names then describes
 * the WRONG session in the one we're about to start:
 *
 * - `CLAUDE_CODE_CHILD_SESSION` makes the new session treat itself as a
 *   child of the caller and stop writing a transcript — which wt reads
 *   for delivery confirmation, status, summaries and the away feed.
 *   Started from a shell it looked perfect; started by an agent (i.e.
 *   almost always) it silently lost all of it.
 * - `CLAUDE_CODE_MESSAGING_SOCKET` points at the CALLER's messaging
 *   endpoint. Same class of bug as the `BUN_INSPECT` inheritance this
 *   file already guards: a child holding a parent's socket path.
 * - the rest are identity/entrypoint markers whose stale values are at
 *   best confusing and at worst load-bearing somewhere we can't see.
 *
 * `NODE_OPTIONS` is deliberately NOT stripped: it is a general-purpose
 * variable a user may set for their own reasons, not Claude's identity.
 */
const CLAUDE_INHERITED_ENV = [
  "CLAUDE_CODE_CHILD_SESSION",
  "CLAUDE_CODE_MESSAGING_SOCKET",
  "CLAUDE_CODE_SESSION_ID",
  "CLAUDE_CODE_BRIDGE_SESSION_ID",
  "CLAUDE_CODE_ENTRYPOINT",
  "CLAUDE_CODE_EXECPATH",
  "CLAUDE_PID",
  "CLAUDECODE",
] as const;

/**
 * bun parses `BUN_INSPECT` as a URL, so a socket path carrying a
 * character that survives URL parsing as an escape (a space becomes
 * `%20`, which bun then tries to bind literally) yields a session with
 * no inspector and a loud stack on its stderr. `homedir()` containing a
 * space is a real case this repo already guards elsewhere, and the
 * cache root lives under it by default.
 *
 * Rather than ship a session that looks fine and can't be messaged, we
 * skip the variable entirely: the session runs, delivery falls back to
 * typing, and `wt doctor` explains why.
 */
function inspectorPathIsUrlSafe(path: string): boolean {
  return !/[\s%#?]/.test(path);
}

/**
 * Ready the inspector socket and PATH shims a Claude session is about
 * to use.
 *
 * Must run BEFORE the session is created, and only when it is actually
 * being created — hence its own step rather than a side effect of argv
 * construction. bun does not unlink an existing socket: it fails the
 * bind with EADDRINUSE and runs on without an inspector, silently
 * costing that session its transport for as long as it lives. So a
 * leftover file from a dead session has to go.
 *
 * The liveness guard is the load-bearing half, and it fails CLOSED. A
 * tmux session already running under this name owns the socket at this
 * path, and removing it would break delivery to a perfectly healthy
 * session — so an unanswerable "is one running?" must not read as "no".
 * `probeSessionNames` returns null exactly when it could not tell
 * (tmux spawn refused under fork pressure, an unreachable socket), and
 * that is precisely the moment this runs most often.
 */
export const prepareInspectorSocket = Effect.fn("prepareInspectorSocket")(function* (
  kind: InnerSessionKind,
  tmuxName: string,
) {
  if (kind !== "claude") return;
  yield* Effect.sync(ensureInspectShims);
  const running = yield* probeSessionNames();
  if (running === null) {
    log.warn("tmux liveness unknown; leaving the inspector socket alone", { tmuxName });
    return;
  }
  if (running.has(tmuxName)) return;
  yield* Effect.sync(() => {
    ensureInspectorDir();
    clearInspectorSocket(tmuxName);
  });
});

/**
 * Strip tmux identity from every inner process, and capture stderr only
 * for harness / diff sessions whose startup errors would otherwise
 * disappear with a short-lived pane. Interactive shells must inherit
 * stderr through the tmux PTY: programs such as Corepack write prompts
 * there and then wait on stdin, so redirecting it makes them look hung.
 *
 * Also stamps the session's identity into its environment:
 *
 * - `BROWSER_CONTROL_SESSION` so any browser tabs an agent opens are
 *   already attributed to this worktree and get closed with it (see
 *   `core/browser.ts`).
 * - `WT_AGENT` — the slug whose AGENT this session is. `wt claude send`
 *   / `wt manager send` read it to stamp the sender on outgoing
 *   messages, which is why agents no longer hand-prefix them (a
 *   convention every agent had to remember, and some didn't). Harness
 *   kinds only, deliberately: a human's `F10` shell in the same
 *   worktree must not send mail signed as that worktree's agent.
 * - `BUN_INSPECT` (claude only) so the session exposes the inspector
 *   socket its messages arrive through — see
 *   `core/harness/claude/inject/transport.ts`. Opening it is inert
 *   until something connects.
 * - `PATH` with this checkout's wt launcher in front (every session),
 *   so an agent on a worker can assert `wt status` without relying on a
 *   machine-global install. Claude additionally receives wt's shim dir,
 *   so bun programs don't inherit `BUN_INSPECT` and fight their parent
 *   for its socket — see `inject/shims.ts`.
 *
 * All of it goes in the `env` prefix rather than tmux's `-e`: tmux's
 * copy is applied to the pane's login shell, which rebuilds parts of
 * the environment — this form is `exec env VAR=… <harness>`, so the
 * harness and everything it spawns inherit it unconditionally. It is
 * also the only form that beats the tmux SERVER's birth environment,
 * which is what an unset variable actually falls back to (not the
 * environment of the client issuing `new-session`).
 *
 * The capture branch uses `exec` to keep the process tree flat, and
 * passes `stderrPath` as `$1` so callers never have to shell-escape it.
 * Shared by attached, detached, and messaged session creation paths so
 * their routing policy cannot drift.
 */
export function wrapInnerArgs(opts: {
  kind: InnerSessionKind;
  stderrPath: string;
  innerArgs: string[];
  slug?: string;
  /**
   * The tmux session name. Claude sessions derive their inspector
   * socket from it; omitting it yields a session with no message
   * transport of its own (deliveries fall back to terminal input), so
   * every creation path should pass it.
   */
  tmuxName?: string;
}): string[] {
  const { kind, stderrPath, innerArgs, slug, tmuxName } = opts;
  const extraEnv: string[] = [];
  // Both identity stamps are unset FIRST and re-added below only when
  // this session earns them. Adding without unsetting looked equivalent
  // and wasn't: wt almost always runs inside a Claude session, so the
  // "no stamp" branches silently inherited the CALLER's identity — a
  // human's F10 shell sending mail signed as whichever agent opened it,
  // and its browser tabs closing with that agent's worktree.
  const unset: string[] = ["TMUX", "TMUX_PANE", "WT_WORKSPACE_SOCKET", "WT_WORKSPACE", "WT_AGENT", "BROWSER_CONTROL_SESSION"];
  const harnessId = harnessIdForKind(kind);
  if (harnessId !== null) {
    // wt is commonly launched from a shell that disables color for command
    // parsing. Interactive harnesses own a terminal UI, so inheriting either
    // spelling strips their styling despite the truecolor tmux setup.
    unset.push("NO_COLOR", "NO_COLOUR");
    if (slug) extraEnv.push(`WT_AGENT=${slug}`);
  }
  const sessionPath = kind === "claude"
    ? `${launcherBinDir()}${delimiter}${pathWithShims()}`
    : `${launcherBinDir()}${delimiter}${process.env.PATH ?? ""}`;
  extraEnv.push(`PATH=${sessionPath}`);
  if (kind === "claude") {
    unset.push(...CLAUDE_INHERITED_ENV);
    if (tmuxName) {
      const socket = inspectorSocketPath(tmuxName);
      if (inspectorPathIsUrlSafe(socket)) {
        extraEnv.push(`BUN_INSPECT=ws+unix://${socket}`);
      } else {
        log.warn("inspector socket path is not URL-safe; session will be typed at", {
          tmuxName,
          socket,
        });
      }
    }
  }
  const envPrefix = [
    "env",
    ...unset.flatMap((name) => ["-u", name]),
    ...(slug ? [`BROWSER_CONTROL_SESSION=${browserSessionName(slug)}`] : []),
    ...extraEnv,
  ];
  if (!capturesInnerStderr(kind)) return [...envPrefix, ...innerArgs];
  return [
    ...envPrefix,
    "bash",
    "-c",
    'p="$1"; shift; exec "$@" 2> "$p"',
    "_wt_wrap",
    stderrPath,
    ...innerArgs,
  ];
}
