/**
 * Local wrapper sessions for REMOTE worktree sessions — the hub-mode
 * bridge across the SSH boundary.
 *
 * A remote worktree's real sessions live on the remote host's own
 * `-L wt` tmux server; the hub's right pane can only `switch-client`
 * to sessions on the LOCAL inner server. The bridge is a local session
 * whose sole program is the same interactive SSH command classic mode
 * hands the raw terminal to (`ssh -t <host> wt _session <slug>
 * <target> <harness>`) — the wrapper IS a local session, so the
 * unmodified `switchRight` shows it like any other. Lifecycle falls
 * out for free: when the SSH drops (host asleep, remote worktree
 * destroyed, remote session exited), the wrapper's program exits, tmux
 * reaps the session, and the hub's shown-session liveness watch resets
 * the pane to home. Killing the wrapper locally merely detaches the
 * remote client — remote work is never at risk from this module.
 *
 * Naming: `wt-remote~<slug>~<target>` (e.g. `wt-remote~eng-42~diff`).
 * `~` is the one character that can't appear in slugs — git forbids it
 * in ref names, so no branch-derived slug ever carries one (the same
 * invariant `CLAUDE_NAMED_SEP` rests on) — which makes both the prefix
 * collision-proof against local sessions AND the trailing `~<target>`
 * split unambiguous for arbitrary REMOTE slugs. (A suffix scheme like
 * `-diff` would collide: remote slugs are unvalidated remote strings,
 * and `<slug>-shell`'s harness wrapper would be indistinguishable from
 * `<slug>`'s shell wrapper.) Like `HUB_HOME_SESSION`, wrapper names
 * are reserved: `classifySessions` carves them out before kind-suffix
 * matching (a wrapper must never surface as a local session for a
 * `wt-remote~…` slug) and the orphan reaper skips them (their slug is
 * remote, so it can never appear in a local live-slug set).
 */
import { homedir } from "node:os";

import type { RemoteConfig } from "../config.ts";
import { createLogger } from "../logger.ts";
import { interactiveRemoteSshArgv } from "../remote.ts";
import { ensureConfig } from "./config.ts";
import { CLAUDE_NAMED_SEP, TMUX_SOCKET } from "./naming.ts";
import { killByName, listAllSessionsRaw } from "./process.ts";

const log = createLogger("[tmux]");

export const REMOTE_WRAPPER_PREFIX = `wt-remote${CLAUDE_NAMED_SEP}`;

export type RemoteSessionTarget = "harness" | "diff" | "shell";

/** One live wrapper session, as classified from a raw tmux name list. */
export type RemoteWrapperEntry = {
  /** The REMOTE worktree's slug (unique per host; one host is configured). */
  slug: string;
  target: RemoteSessionTarget;
  /** Full local tmux session name (`switch-client` / kill target). */
  name: string;
};

const TARGETS = ["harness", "diff", "shell"] as const;

/** Local tmux session name wrapping `<slug>`'s remote `<target>` session. */
export function remoteWrapperName(
  slug: string,
  target: RemoteSessionTarget,
): string {
  return `${REMOTE_WRAPPER_PREFIX}${slug}${CLAUDE_NAMED_SEP}${target}`;
}

/**
 * Parse a raw tmux session name as a wrapper; null when it isn't one
 * (including a `wt-remote~`-prefixed name whose trailing `~<target>`
 * segment isn't a known target — better an ignored session than a
 * misattributed slug).
 */
export function parseRemoteWrapper(name: string): RemoteWrapperEntry | null {
  if (!name.startsWith(REMOTE_WRAPPER_PREFIX)) return null;
  const rest = name.slice(REMOTE_WRAPPER_PREFIX.length);
  const sep = rest.lastIndexOf(CLAUDE_NAMED_SEP);
  if (sep <= 0) return null;
  const slug = rest.slice(0, sep);
  const target = rest.slice(sep + 1);
  if (!(TARGETS as readonly string[]).includes(target)) return null;
  return { slug, target: target as RemoteSessionTarget, name };
}

/**
 * Ensure the wrapper session for (slug, target) exists, creating it
 * detached when it doesn't — the remote counterpart of
 * `ensureSessionDetached`, minus every local concern (no cwd in a
 * worktree, no pipe-pane, no shortcut tag: the F-key switch bindings
 * are for local worktree sessions, and hub mode's outer server owns
 * the F-keys anyway). Same TOCTOU tolerance as the local ensure: a
 * non-zero create that nevertheless left the session live (duplicate
 * race) reports success.
 */
export async function ensureRemoteWrapperSession(opts: {
  remote: RemoteConfig;
  slug: string;
  target: RemoteSessionTarget;
  harnessId: string;
}): Promise<{ ok: true; name: string; created: boolean } | { ok: false; reason: string }> {
  const { remote, slug, target, harnessId } = opts;
  const name = remoteWrapperName(slug, target);
  const configPath = ensureConfig();

  const exists = (await listAllSessionsRaw().catch(() => new Set<string>())).has(name);
  if (exists) return { ok: true, name, created: false };

  const proc = Bun.spawn(
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
      ...interactiveRemoteSshArgv(remote, ["_session", slug, target, harnessId]),
    ],
    // homedir, same as `tmuxClientCwd` (not imported: attach.ts is on
    // an import cycle with admin.ts, which imports this module) — the
    // first client to touch the socket forks the server and pins its
    // cwd for life, so clients never run from a destructible worktree.
    { cwd: homedir(), stdout: "ignore", stderr: "pipe" },
  );
  const [code, stderr] = await Promise.all([
    proc.exited,
    new Response(proc.stderr).text(),
  ]);
  if (code !== 0) {
    const reason = stderr.trim() || `tmux new-session exited ${code}`;
    const nowExists = (await listAllSessionsRaw().catch(() => new Set<string>())).has(name);
    if (nowExists) {
      log.warn("ensureRemoteWrapperSession: create raced but session exists", {
        slug,
        target,
        code,
        reason,
      });
      return { ok: true, name, created: false };
    }
    log.warn("ensureRemoteWrapperSession: create failed", { slug, target, code, reason });
    return { ok: false, reason };
  }
  log.info(`created remote wrapper session ${name}`, { host: remote.host });
  return { ok: true, name, created: true };
}

/**
 * Kill every wrapper session for a remote slug. Detaches this
 * machine's view of the remote sessions — the remote host's own tmux
 * sessions keep running untouched.
 */
export async function killRemoteWrapperSessions(slug: string): Promise<void> {
  await Promise.allSettled(
    TARGETS.map((t) => killByName(remoteWrapperName(slug, t))),
  );
}
