import { killActionSession } from "./action-sessions.ts";
import type { HarnessId } from "../harness/index.ts";
import { createLogger } from "../logger.ts";
import { run } from "../proc.ts";
import {
  bareSlug,
  CLAUDE_NAMED_SEP,
  HUB_HOME_SESSION,
  sessionName,
  SUFFIX,
  TMUX_SOCKET,
} from "./naming.ts";
import { killByName, listAllSessionsRaw } from "./process.ts";

const log = createLogger("[tmux]");

/**
 * Kill the entire wt tmux server (every session). Idempotent — exits
 * 0 when no server is running, after warning to stderr we discard.
 */
export async function killServer(): Promise<void> {
  await run(["tmux", "-L", TMUX_SOCKET, "kill-server"]);
}

/**
 * Kill one worktree's primary claude session. Idempotent — silently
 * no-ops when the session doesn't exist or the server isn't running.
 * Other kinds and any *named* claude sessions on the same slug are
 * unaffected; use `killClaudeNamedSession` / `killDiffSession` /
 * `killShellSession` for those, or `killAllSessionsFor` to drop
 * every kind at once.
 */
export async function killSession(slug: string): Promise<void> {
  await killByName(sessionName(slug, "claude"));
}

/** Kill one worktree's named (non-primary) claude session. Idempotent. */
export async function killClaudeNamedSession(
  slug: string,
  claudeName: string,
): Promise<void> {
  await killByName(sessionName(slug, "claude", claudeName));
}

/**
 * Kill one worktree's harness session by id. For Claude with a non-null
 * managedName, kills the named session; otherwise kills the primary
 * tmux slot for that harness on that slug. Idempotent.
 */
export async function killHarnessSession(
  slug: string,
  harnessId: HarnessId,
  managedName: string | null = null,
): Promise<void> {
  await killByName(sessionName(slug, harnessId, managedName));
}

/**
 * Gracefully end a harness session by typing the harness's own exit
 * gesture into its pane — Ctrl+D twice, the same "I'm done with this
 * convo" keys you'd press inside claude — rather than `kill-session`
 * yanking the slot out from under it. The harness shuts down cleanly
 * (conversation persisted, terminal restored) and the tmux session
 * ends when its command exits. Best-effort by design: a harness with
 * text in its input box ignores EOF, so the session just stays up and
 * nothing is lost. No-ops on a missing session (send-keys just fails;
 * `run` swallows the exit code).
 */
export async function closeHarnessSessionGracefully(
  slug: string,
  harnessId: HarnessId,
  managedName: string | null = null,
): Promise<void> {
  const name = sessionName(slug, harnessId, managedName);
  // `=${name}` alone is a valid SESSION target (kill-session) but
  // send-keys resolves a PANE target, where the bare exact-match form
  // errors with "can't find pane". The trailing `:` makes it
  // exact-session + active-window, which pane resolution accepts.
  const send = () =>
    run(["tmux", "-L", TMUX_SOCKET, "send-keys", "-t", `=${name}:`, "C-d"]);
  await send();
  // A beat between the two presses: claude arms its "press ctrl+d
  // again to exit" confirm on the first and needs a render tick before
  // the second registers as the confirmation.
  await new Promise((r) => setTimeout(r, 200));
  await send();
}

/** Kill one worktree's diff session. Idempotent. */
export async function killDiffSession(slug: string): Promise<void> {
  await killByName(sessionName(slug, "diff"));
}

/** Kill one worktree's shell session. Idempotent. */
export async function killShellSession(slug: string): Promise<void> {
  await killByName(sessionName(slug, "shell"));
}

// Action session kills go through `core/tmux/action-sessions.ts` —
// `killAllSessionsFor` below imports the sync helper there. Keeping a
// duplicate definition here would invite drift between two sources of
// truth for the same tmux command.

/**
 * Kill every kind of session for a slug (claude primary + every
 * named claude, diff, shell, action). Used by destroy paths so none
 * linger with cwd inside a half-deleted worktree.
 */
export async function killAllSessionsFor(slug: string): Promise<void> {
  // List once and pick out any session whose bareSlug matches —
  // covers primary, named claudes, diff, shell, and action without
  // hardcoding the named-claude list. The action kill goes via
  // action-sessions.ts (now async like the rest).
  const all = await listAllSessionsRaw().catch(() => new Set<string>());
  const ours = [...all].filter((n) => bareSlug(n) === slug);
  await Promise.allSettled([
    ...ours.map((n) => killByName(n)),
    killActionSession(slug),
  ]);
}

const BASE_PLACEHOLDER = "{{base}}";

/** Whether the user's `[diff].command` template depends on the diff base. */
export function diffCommandUsesBase(template: string): boolean {
  return template.includes(BASE_PLACEHOLDER);
}

/**
 * Substitute `{{base}}` in the user's diff command template with the
 * resolved base ref. The ref is wrapped in double quotes so refs
 * containing characters that the user's shell would otherwise expand
 * (e.g. globs in oddly-named local branches) survive intact.
 *
 * Injection note (audited, accepted): the double-quote escape leaves
 * `$`/backtick live, but it is NOT the load-bearing safety — every
 * caller resolves the base through `effectiveBaseOrTrunk` first, whose
 * rev-parse gate rejects anything that isn't a real commit-ish. Don't
 * route an unvalidated ref into this template without hardening the
 * escape to single-quote (`shQuote`) form first.
 *
 * Templates that don't reference `{{base}}` pass through unchanged so
 * users with custom diff commands (`gitu`, `lazygit`, …) keep working.
 * Templates that do reference it but receive no base resolve the
 * placeholder to the empty string so the user's shell surfaces the
 * resulting parse error visibly rather than us silently masking the
 * misuse.
 */
export function resolveDiffCommand(template: string, base: string | undefined): string {
  if (!diffCommandUsesBase(template)) return template;
  const ref = base ? `"${base.replaceAll('"', '\\"')}"` : "";
  return template.replaceAll(BASE_PLACEHOLDER, ref);
}

/**
 * One live claude session as seen by tmux. `name = null` is the
 * primary (tmux session name = bare slug); a string is a user-named
 * additional session (tmux session name = `<slug>~<name>`).
 */
export type ClaudeSessionEntry = { slug: string; name: string | null };

/** Classified-by-kind view of a set of raw tmux session names, minus `all`. */
export type SessionClassification = {
  claude: ClaudeSessionEntry[];
  claudeSlugs: Set<string>;
  codex: Set<string>;
  opencode: Set<string>;
  diff: Set<string>;
  shell: Set<string>;
  action: Set<string>;
};

/**
 * Pure classifier behind `listSessions` — split out so the
 * (name → kind) logic is unit-testable without spawning a real tmux
 * server. See `listSessions` for the full semantics; the one thing to
 * know here is that `HUB_HOME_SESSION` (the reserved hub dashboard
 * session, not a worktree session of any kind) is excluded from every
 * returned set, including as a bare-slug claude entry.
 */
export function classifySessions(names: Iterable<string>): SessionClassification {
  const claude: ClaudeSessionEntry[] = [];
  const claudeSlugs = new Set<string>();
  const codex = new Set<string>();
  const opencode = new Set<string>();
  const diff = new Set<string>();
  const shell = new Set<string>();
  const action = new Set<string>();
  for (const name of names) {
    // The reserved hub-home dashboard session isn't a worktree session
    // of any kind — it must not surface as a claude session for slug
    // "wt-hub-home" (or any other classified set). See naming.ts.
    if (name === HUB_HOME_SESSION) continue;
    if (name.endsWith(SUFFIX.codex)) {
      codex.add(name.slice(0, -SUFFIX.codex.length));
    } else if (name.endsWith(SUFFIX.opencode)) {
      opencode.add(name.slice(0, -SUFFIX.opencode.length));
    } else if (name.endsWith(SUFFIX.diff)) {
      diff.add(name.slice(0, -SUFFIX.diff.length));
    } else if (name.endsWith(SUFFIX.shell)) {
      shell.add(name.slice(0, -SUFFIX.shell.length));
    } else if (name.endsWith(SUFFIX.action)) {
      action.add(name.slice(0, -SUFFIX.action.length));
    } else {
      const tildeIdx = name.lastIndexOf(CLAUDE_NAMED_SEP);
      if (tildeIdx > 0) {
        const slug = name.slice(0, tildeIdx);
        const claudeName = name.slice(tildeIdx + 1);
        claude.push({ slug, name: claudeName });
        claudeSlugs.add(slug);
      } else {
        claude.push({ slug: name, name: null });
        claudeSlugs.add(name);
      }
    }
  }
  return { claude, claudeSlugs, codex, opencode, diff, shell, action };
}

/**
 * Bare slug sets (and named-claude entries) for the live sessions of
 * each kind. Partitioned so the indicators, kill-confirm hints, and
 * the sessions picker can each read what they need independently.
 * One CLI call regardless of worktree count.
 *
 * `claude` is a list of `(slug, name)` because a single worktree can
 * host multiple claude sessions (primary + N named). `codex` and
 * `opencode` are slug sets — for v1 they're single-tmux-per-slug.
 * The legacy `claudeSlugs` set is the unique-slug projection of
 * `claude` — preserved so "row has any live claude" checks stay a
 * Set lookup.
 *
 * Server-not-running exits non-zero with a "no server running"
 * stderr; we map that to empty sets rather than throwing — it's the
 * steady state when no worktree has been entered yet.
 */
export async function listSessions(): Promise<
  SessionClassification & {
    /** Raw set of every live tmux session name. Used by harness impls
     *  to compute `isLive` without a second `list-sessions` call. */
    all: Set<string>;
  }
> {
  const all = await listAllSessionsRaw();
  return { ...classifySessions(all), all };
}

/**
 * Reconcile sessions against a live slug set. Kills any session of
 * any kind (claude, diff, or shell) whose underlying slug isn't in
 * `liveSlugs` — covers the case where a worktree was destroyed (in
 * this wt run or a prior one) without our session-kill hook firing.
 * The bare slug is derived by stripping the kind suffix so every
 * kind is reaped for a removed worktree. Errors are swallowed; an
 * orphaned session is a worse outcome than blocking startup.
 */
export async function reapOrphanedSessions(
  liveSlugs: ReadonlySet<string>,
): Promise<void> {
  let sessions: Set<string>;
  try {
    sessions = await listAllSessionsRaw();
  } catch {
    return;
  }
  // The reserved hub-home session isn't slug-owned, so it can never
  // appear in a caller's `liveSlugs` set on its own merits — whitelist
  // it directly rather than relying on every caller to remember to add
  // it alongside the slot-slug whitelist.
  const orphans = [...sessions].filter(
    (s) => s !== HUB_HOME_SESSION && !liveSlugs.has(bareSlug(s)),
  );
  if (orphans.length === 0) return;
  log.info(`reaping ${orphans.length} orphaned tmux session(s)`, {
    orphans,
  });
  await Promise.allSettled(orphans.map((name) => killByName(name)));
}
