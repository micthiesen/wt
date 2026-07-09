import { join } from "node:path";

import { getHarness, type HarnessId } from "../harness/index.ts";
import { createLogger } from "../logger.ts";
import { sessionsDir } from "./attach.ts";
import { ensureConfig } from "./config.ts";
import { sessionName, TMUX_SOCKET } from "./naming.ts";
import { capturePane, listAllSessionsRaw, paneTarget, runTmux } from "./process.ts";

const log = createLogger("[tmux]");

const sleep = (ms: number): Promise<void> =>
  new Promise((resolve) => setTimeout(resolve, ms));

/** Named tmux paste buffer used by `injectIntoSession`. */
const INJECT_BUFFER = "wt-inject";
/** Settle pause when the session is already running before pasting. */
const WARM_SETTLE_MS = 300;
/** capture-pane poll interval while waiting for a cold start to render. */
const READY_POLL_MS = 350;
/** Hard cap on the cold-start readiness wait; inject anyway after this. */
const READY_MAX_MS = 12_000;
/** Gap between the paste landing and the Enter that submits it. */
const SUBMIT_DELAY_MS = 500;
/** Gap between successive submit keys (e.g. claude's double Enter). */
const SUBMIT_KEY_GAP_MS = 250;

/**
 * Wait until a freshly-started harness pane stops changing — meaning it
 * has finished its initial render and is sitting at an idle prompt — or
 * the cap elapses. Stability (two identical, non-trivial snapshots) is
 * version-agnostic: we never scrape the harness's exact prompt string, we
 * just watch for the screen to settle. A startup spinner keeps the pane
 * changing, so the loop naturally waits out a slow boot instead of
 * guessing a fixed delay. Returns whether it settled (false = hit the
 * cap; the caller pastes anyway).
 */
async function waitForPaneReady(name: string): Promise<boolean> {
  const deadline = Date.now() + READY_MAX_MS;
  let prev: string | null = null;
  // Initial grace — harnesses often write nothing for the first beat after spawn.
  await sleep(READY_POLL_MS);
  while (Date.now() < deadline) {
    const cur = (await capturePane(name))?.trim() ?? "";
    if (cur.length > 0 && cur === prev) return true;
    prev = cur;
    await sleep(READY_POLL_MS);
  }
  return false;
}

/**
 * Create the worktree's primary harness session detached (no client
 * attach). Byte-for-byte the session `attachOrCreate({kind:harnessId})`
 * would make — same name, same `buildArgs` argv, same stderr
 * wrapper and TMUX-stripping env — so a later F12 `new-session -A` just
 * attaches to this one rather than spawning a second. Sized generously
 * so the harness doesn't render cramped before the user attaches; tmux
 * resizes to the client on attach.
 */
async function startHarnessSessionDetached(
  slug: string,
  cwd: string,
  harnessId: HarnessId,
): Promise<{ ok: boolean; reason?: string }> {
  const harness = getHarness(harnessId);
  const name = sessionName(slug, harnessId);
  // ensureConfig, NOT writeConfig: this can run from inside the wt tmux
  // server (e.g. `wt claude send` issued by another claude session),
  // where the rendered config differs and the kill-server-on-change
  // dance would take down every live session including the caller's.
  const configPath = ensureConfig();
  const stderrPath = join(sessionsDir(), `${name}.err`);
  const innerArgs = harness.buildArgs({
    wtPath: cwd,
    managedName: null,
    resumeSessionId: null,
  });
  let proc: Bun.Subprocess;
  try {
    proc = Bun.spawn(
      [
        "tmux",
        "-L",
        TMUX_SOCKET,
        "-f",
        configPath,
        "new-session",
        "-d",
        "-s",
        name,
        "-c",
        cwd,
        "-x",
        "200",
        "-y",
        "50",
        // See attachOrCreate header: claude downgrades to 256-color when
        // $TMUX is set, so strip it before exec'ing. The bash wrapper
        // redirects stderr to a file so a spawn-and-die surfaces a reason.
        "env",
        "-u",
        "TMUX",
        "-u",
        "TMUX_PANE",
        "bash",
        "-c",
        'p="$1"; shift; exec "$@" 2> "$p"',
        "_wt_wrap",
        stderrPath,
        ...innerArgs,
      ],
      {
        cwd,
        stdout: "ignore",
        stderr: "pipe",
        env: {
          ...process.env,
          TERM: process.env.TERM ?? "xterm-256color",
          COLORTERM: process.env.COLORTERM ?? "truecolor",
          FORCE_COLOR: process.env.FORCE_COLOR ?? "3",
        },
      },
    );
  } catch (err) {
    const reason = err instanceof Error ? err.message : String(err);
    log.error("detached harness start spawn failed", {
      slug,
      harnessId,
      reason,
    });
    return { ok: false, reason };
  }
  const [code, stderr] = await Promise.all([
    proc.exited,
    new Response(proc.stderr as ReadableStream<Uint8Array>).text(),
  ]);
  if (code !== 0) {
    const reason = stderr.trim() || `tmux new-session exited ${code}`;
    log.warn("detached harness start failed", {
      slug,
      harnessId,
      code,
      reason,
    });
    return { ok: false, reason };
  }
  return { ok: true };
}

/** Pipe text into the inject buffer and paste it into a session's pane. */
async function pasteBuffer(name: string, text: string): Promise<void> {
  // load-buffer reads stdin, so arbitrary text (quotes, `$`, newlines)
  // needs no shell escaping.
  const load = Bun.spawn(
    ["tmux", "-L", TMUX_SOCKET, "load-buffer", "-b", INJECT_BUFFER, "-"],
    {
      stdin: new TextEncoder().encode(text),
      stdout: "ignore",
      stderr: "ignore",
    },
  );
  // Exit code deliberately unchecked (audited, accepted): a failed load
  // (e.g. the server died between the liveness check and here) means
  // the following paste/Enter lands on an empty buffer — visible in the
  // pane and recoverable — whereas failing the whole inject on a
  // transient tmux hiccup is worse. Revisit if a silent empty submit
  // ever actually bites.
  await load.exited;
  // `-p` = bracketed paste (claude receives it as one chunk, so internal
  // newlines and a leading slash command don't submit early); `-d` drops
  // the buffer after.
  await runTmux([
    "paste-buffer",
    "-d",
    "-p",
    "-b",
    INJECT_BUFFER,
    "-t",
    paneTarget(name),
  ]);
}

/**
 * Send `text` to a worktree's primary (F12) harness session as if typed
 * at the prompt, then submit it. Starts the session first if it isn't
 * running, waiting for it to finish booting before pasting. The prompt
 * lands in the live conversation — with its existing context and history
 * — rather than a fresh headless action run.
 *
 * Fire-and-forget by nature: there's no completion sentinel, so callers
 * can't observe when the harness finishes.
 *
 * Known edge: a brand-new worktree directory the harness has never run in may
 * show its trust prompt on cold start; the paste+Enter would answer that
 * dialog instead of submitting. Attaching via F12 once (to accept trust)
 * before injecting avoids it.
 */
export async function injectIntoSession(opts: {
  slug: string;
  cwd: string;
  harnessId?: HarnessId;
  text: string;
}): Promise<{ ok: true; coldStarted: boolean } | { ok: false; reason: string }> {
  const { slug, cwd, text } = opts;
  const harnessId = opts.harnessId ?? "claude";
  const name = sessionName(slug, harnessId);
  const running = (
    await listAllSessionsRaw().catch(() => new Set<string>())
  ).has(name);
  let coldStarted = false;
  if (!running) {
    const started = await startHarnessSessionDetached(slug, cwd, harnessId);
    if (!started.ok) {
      return {
        ok: false,
        reason: started.reason ?? `failed to start ${harnessId} session`,
      };
    }
    coldStarted = true;
    await waitForPaneReady(name);
  } else {
    await sleep(WARM_SETTLE_MS);
  }
  try {
    await pasteBuffer(name, text);
    await sleep(SUBMIT_DELAY_MS);
    // Harnesses declare their own submit-key sequence: most take a
    // single Enter, but Claude Code and Codex receive the bracketed
    // paste as a multi-line input blob whose first Enter only exits
    // that state, so they need a second to actually submit. Keys are
    // sent in order with a small gap so the harness processes each
    // before the next lands.
    const submitKeys = getHarness(harnessId).injectSubmitKeys;
    for (let i = 0; i < submitKeys.length; i++) {
      if (i > 0) await sleep(SUBMIT_KEY_GAP_MS);
      const { code, stderr } = await runTmux([
        "send-keys",
        "-t",
        paneTarget(name),
        submitKeys[i]!,
      ]);
      if (code !== 0) {
        return {
          ok: false,
          reason: stderr.trim() || `tmux send-keys exited ${code}`,
        };
      }
    }
  } catch (err) {
    return {
      ok: false,
      reason: err instanceof Error ? err.message : String(err),
    };
  }
  return { ok: true, coldStarted };
}
