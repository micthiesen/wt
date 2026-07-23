/**
 * Build + launch the hub server's two-pane layout.
 *
 * `ensureHubLayout` is the idempotent "make it so" call: create the
 * `hub` session with its two panes if it doesn't exist (or exists
 * malformed), otherwise no-op. `launchHub` is the `wt hub` entry point:
 * it additionally handles the config-changed-so-rebuild-the-server
 * dance and the final interactive attach.
 */
import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

import { createLogger } from "../logger.ts";
import { ensureConfig } from "../tmux/config.ts";
import { HUB_HOME_SESSION, shQuote } from "../tmux/naming.ts";
import { TMUX_SOCKET } from "../tmux.ts";
import { writeHubConfig } from "./config.ts";
import { HUB_SESSION, HUB_SOCKET, WT_HUB_ENV, WT_HUB_ENV_VALUE } from "./naming.ts";
import { spawnTmux } from "./proc.ts";

const log = createLogger("[hub]");

/**
 * Argv prefix that re-invokes this exact wt build — used to spawn
 * fresh `wt _taskpane` / `wt _home` processes without hardcoding a
 * launcher path. Preferred resolution is the repo's `src/main.ts`
 * located relative to THIS module — immune to how the current process
 * was started (bin/wt, `bun src/main.ts`, a test runner, some other
 * script that imported core/hub). Falls back to re-splicing
 * `process.argv[0..1]` for hypothetical layouts where the source tree
 * isn't on disk next to this file (e.g. a future bundled build), where
 * argv[1] is the equivalent leading element the invocation carried.
 * Callers append their own subcommand after this prefix, e.g.
 * `[...wtArgv(), "_taskpane"]`.
 */
export function wtArgv(): string[] {
  const main = join(import.meta.dir, "..", "..", "main.ts");
  if (existsSync(main)) return [process.execPath, main];
  const exe = process.argv[0];
  const script = process.argv[1];
  if (!exe) return [process.execPath];
  return script ? [exe, script] : [process.execPath];
}

/**
 * Shell command string for the right pane's split-window: a nested
 * tmux client (env-stripped of `TMUX`/`TMUX_PANE`/`WT_HUB` so it isn't
 * confused for a pane of its own parent) attaching-or-creating the
 * reserved `wt-hub-home` session on the INNER `-L wt` server.
 * `WT_HUB` matters here for a subtler reason than `TMUX`/`TMUX_PANE`:
 * the hub session sets `-e WT_HUB=1` session-scoped on itself (see
 * `ensureHubLayout` below), but that's inherited by every process this
 * pane forks — including, on whichever hub launch happens to be first
 * to touch the inner `-L wt` socket, the tmux client that FORKS the
 * inner server itself. A forked server inherits its birth client's
 * environment as its own global env for the rest of its life, so
 * without the strip every session ever created on that inner server
 * would see `WT_HUB=1` and `isHubPane()` would report true everywhere,
 * permanently. `-u WT_HUB` closes that hole at the source. `new-session
 * -A` means the inner server boots lazily on first hub launch rather
 * than requiring it to already be running. Every token is `shQuote`d
 * (not just the paths) since this whole thing travels as ONE argv
 * element that tmux itself re-parses through the shell.
 */
function rightPaneCommand(): string {
  const configPath = ensureConfig();
  const argv = [
    "env",
    "-u",
    "TMUX",
    "-u",
    "TMUX_PANE",
    "-u",
    WT_HUB_ENV,
    "tmux",
    "-L",
    TMUX_SOCKET,
    "-f",
    configPath,
    "new-session",
    "-A",
    "-s",
    HUB_HOME_SESSION,
    "-c",
    homedir(),
    ...wtArgv(),
    "_home",
  ];
  return argv.map(shQuote).join(" ");
}

/**
 * Ensure the `hub` session exists with its two-pane layout. Idempotent:
 * a healthy existing session (2 panes) is left alone; a session that
 * exists but doesn't have exactly 2 panes (killed pane, manual tmux
 * fiddling, a previous crash mid-setup) is torn down and rebuilt rather
 * than patched in place — recreating from scratch is simpler than
 * reasoning about partial layouts.
 *
 * Throws if `split-window` fails after a fresh `new-session` — the
 * just-created (broken, one-pane) session is killed first so a failed
 * call never leaves partial state behind for a later caller to trip
 * over. Callers that need a non-throwing degrade (rather than
 * `launchHub`'s "print and exit non-zero") should catch explicitly.
 */
export async function ensureHubLayout(): Promise<void> {
  const has = await spawnTmux(HUB_SOCKET, ["has-session", "-t", HUB_SESSION]);
  if (has.code === 0) {
    const panes = await spawnTmux(HUB_SOCKET, [
      "list-panes",
      "-t",
      `${HUB_SESSION}:0`,
      "-F",
      "#{pane_id}",
    ]);
    const paneCount = panes.stdout
      .split("\n")
      .map((l) => l.trim())
      .filter(Boolean).length;
    if (paneCount === 2) return;
    log.warn("hub session malformed; recreating", { paneCount });
    await spawnTmux(HUB_SOCKET, ["kill-session", "-t", HUB_SESSION]);
  }

  const { path: hubConfPath } = writeHubConfig();
  const home = homedir();

  const create = await spawnTmux(HUB_SOCKET, [
    "-f",
    hubConfPath,
    "new-session",
    "-d",
    "-s",
    HUB_SESSION,
    "-x",
    "-",
    "-y",
    "-",
    "-e",
    `${WT_HUB_ENV}=${WT_HUB_ENV_VALUE}`,
    "-c",
    home,
    ...wtArgv(),
    "_taskpane",
  ]);
  if (create.code !== 0) {
    log.warn("hub new-session failed", { stderr: create.stderr.trim() || null });
    return;
  }

  const split = await spawnTmux(HUB_SOCKET, [
    "-f",
    hubConfPath,
    "split-window",
    "-h",
    "-t",
    `${HUB_SESSION}:0`,
    "-c",
    home,
    rightPaneCommand(),
  ]);
  if (split.code !== 0) {
    // Proceeding to resize/select below would leave (and then attach)
    // a one-pane session masquerading as the two-pane hub layout —
    // silently broken rather than loudly failed. Kill the
    // just-created session and throw instead; `launchHub` surfaces
    // this to the user and exits non-zero rather than attaching it.
    const stderr = split.stderr.trim() || null;
    log.warn("hub split-window failed; tearing down partial session", { stderr });
    await spawnTmux(HUB_SOCKET, ["kill-session", "-t", HUB_SESSION]);
    throw new Error(`hub split-window failed: ${stderr ?? `exit ${split.code}`}`);
  }

  await spawnTmux(HUB_SOCKET, ["resize-pane", "-t", `${HUB_SESSION}:0.0`, "-x", "35"]);
  // Focus starts on the TASK pane: the hub opens as an inbox to triage,
  // and `useHubPaneFocus` initializes to focused to match. Enter/F12 on
  // a task hands focus to the session pane the moment there's something
  // to type into.
  await spawnTmux(HUB_SOCKET, ["select-pane", "-t", `${HUB_SESSION}:0.0`]);
}

/**
 * The `wt hub` entry point. Rebuilds the hub server when its config
 * changed (tmux only loads config at server start, so this is the only
 * way an updated binding/quoting takes effect) — cheap, because the hub
 * server holds only layout, never harness state; every real session
 * lives on the untouched inner `-L wt` server. Then attaches
 * interactively, with `TMUX`/`TMUX_PANE` stripped from the spawn env so
 * `wt hub` works even when run from inside the user's own personal
 * tmux. Resolves to the attach client's exit code once the user
 * detaches or the session ends — or to a non-zero code without
 * attaching at all if `ensureHubLayout` couldn't build a working
 * two-pane session (see its split-window failure handling); attaching
 * to a broken layout would be worse than a loud CLI failure.
 */
export async function launchHub(): Promise<number> {
  const { changed } = writeHubConfig();
  if (changed) {
    await spawnTmux(HUB_SOCKET, ["kill-server"]);
  }
  try {
    await ensureHubLayout();
  } catch (err) {
    console.error(`wt hub: failed to build the hub layout: ${(err as Error).message}`);
    return 1;
  }

  const { path: hubConfPath } = writeHubConfig();
  const env = { ...process.env };
  delete env.TMUX;
  delete env.TMUX_PANE;

  const proc = Bun.spawn(
    ["tmux", "-L", HUB_SOCKET, "-f", hubConfPath, "attach", "-t", HUB_SESSION],
    {
      cwd: homedir(),
      stdin: "inherit",
      stdout: "inherit",
      stderr: "inherit",
      env,
    },
  );
  return proc.exited;
}
