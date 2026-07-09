import { createLogger } from "../logger.ts";
import { run } from "../proc.ts";
import { TMUX_SOCKET } from "./naming.ts";

const log = createLogger("[tmux]");

export async function killByName(name: string): Promise<void> {
  const r = await run(["tmux", "-L", TMUX_SOCKET, "kill-session", "-t", `=${name}`]);
  // tmux exits non-zero for "session not found" (the desired no-op
  // path when killing an absent slot) but ALSO for connection /
  // permission failures. Filter the benign case so the noise floor
  // is low, but surface real errors so a failed kill doesn't look
  // like silent success.
  if (r.exitCode !== 0 && !/can't find session/i.test(r.stderr)) {
    log.warn("tmux kill-session failed", {
      name,
      code: r.exitCode,
      stderr: r.stderr.trim() || null,
    });
  }
}

/**
 * Every session name on our private tmux server, including the
 * `<slug>-diff` ones. Used by the reaper and by `attachOrCreate`'s
 * post-detach existence check, which need exact-name matching
 * regardless of kind.
 */
export async function listAllSessionsRaw(): Promise<Set<string>> {
  const r = await run([
    "tmux",
    "-L",
    TMUX_SOCKET,
    "list-sessions",
    "-F",
    "#{session_name}",
  ]);
  if (r.exitCode !== 0) return new Set();
  const names = r.stdout
    .split("\n")
    .map((l) => l.trim())
    .filter(Boolean);
  return new Set(names);
}

/**
 * Exact-match target for the *pane* commands below (capture-pane,
 * paste-buffer, send-keys). Their `-t` is a target-pane, where the bare
 * `=name` exact-session prefix that works for `kill-session` is rejected
 * with "can't find pane". The form that both targets the session's active
 * pane AND keeps exact (non-prefix) matching is `=<name>:` — the trailing
 * colon selects the session's current window. Don't drop the colon.
 */
export function paneTarget(name: string): string {
  return `=${name}:`;
}

/** Run a tmux command on our private server; collect exit code + stderr. */
export async function runTmux(
  args: readonly string[],
): Promise<{ code: number; stderr: string }> {
  const r = await run(["tmux", "-L", TMUX_SOCKET, ...args]);
  return { code: r.exitCode, stderr: r.stderr };
}

/** Snapshot a session's active pane as plain text, or null on failure. */
export async function capturePane(name: string): Promise<string | null> {
  const r = await run([
    "tmux",
    "-L",
    TMUX_SOCKET,
    "capture-pane",
    "-p",
    "-t",
    paneTarget(name),
  ]);
  return r.exitCode === 0 ? r.stdout : null;
}
