/**
 * Claude Code harness impl. Wraps the existing claude-* modules so the
 * old machinery (deterministic UUIDs, jsonl tail, registry status,
 * persisted names) keeps working while the rest of the app talks to
 * the generic Harness interface.
 *
 * Tmux session name format is preserved verbatim: bare slug for the
 * primary session, `<slug>~<name>` for named. Any session the user
 * already has running stays attachable across the upgrade.
 *
 * Summaries (ai-title / away-summary / last-prompt) are NOT folded in
 * at the harness layer — they're a separate `claudeSummariesQuery` in
 * the state layer that the sessions picker merges in at consume time.
 * Keeping the summary fetch out of `discoverSessions` keeps this fn
 * fast and lets the picker render the entry list before the summary
 * scan resolves.
 */
import { claudeStatus, wtSessionArgs, wtSessionUuid } from "../claude.ts";
import { readRegistry } from "../claude-registry.ts";
import {
  buildClaudeSessionEntries,
  listClaudeNames,
  reapClaudeNames,
} from "../claude-sessions.ts";

import type {
  Harness,
  HarnessSession,
  HarnessSpawnArgs,
} from "./types.ts";

/**
 * Separator between slug and named-claude name in the tmux session
 * name. Mirrors `CLAUDE_NAMED_SEP` in `core/tmux.ts` — duplicated here
 * to avoid a circular import (tmux.ts pulls in the harness registry,
 * which pulls in this file).
 */
const CLAUDE_NAMED_SEP = "~";

/**
 * Tmux session name for a claude session. Primary (`name = null`) is
 * the bare slug; named is `<slug>~<name>`. Exported for the few
 * call-sites outside the harness layer that still spell it out.
 */
export function claudeTmuxName(slug: string, name: string | null): string {
  return name === null ? slug : `${slug}${CLAUDE_NAMED_SEP}${name}`;
}

/**
 * `nf-fa-anthropic` would be ideal but isn't in the patched set; the
 * brain glyph reads as "AI/thought" and matches the comment-bubble
 * motif Claude already used in the row. Color is the existing
 * `theme.accent` (orange) — inlined here so core/ doesn't import from
 * tui/.
 */
const CLAUDE_GLYPH = "\u{F0335}"; // nf-md-brain
const CLAUDE_COLOR = "#c47b3a";

export const claudeHarness: Harness = {
  id: "claude",
  label: "Claude Code",
  letter: "c",
  glyph: CLAUDE_GLYPH,
  color: CLAUDE_COLOR,

  tmuxSessionName(slug, managedName) {
    return claudeTmuxName(slug, managedName);
  },

  async discoverSessions({ slug, wtPath }) {
    const status = await claudeStatus({ slug, path: wtPath });
    const tailByName = new Map(status.sessions.map((t) => [t.name, t]));
    const registryStatusBySessionId: Record<string, "busy" | "idle"> = {};
    for (const r of readRegistry()) {
      registryStatusBySessionId[r.sessionId] = r.status;
    }
    const entries = buildClaudeSessionEntries({
      slug,
      wtPath,
      // Liveness is computed in `useHarnessSessions` from the live
      // tmux name set, not here. Passing an empty list means
      // `buildClaudeSessionEntries` won't add tmux-live-but-not-
      // persisted ghost names — see the gap noted on
      // `Harness.discoverSessions`.
      liveNames: [],
      tailByName,
      registryStatusBySessionId,
      // Summaries are merged in by the consumer (sessions picker) via
      // a separate query — discoverSessions stays fast and synchronous-
      // ish so the row can render the F12-target without waiting for
      // jsonl scans.
      summaryBySessionId: {},
    });
    const out: HarnessSession[] = entries.map((e) => ({
      displayName: e.name === null ? "primary" : e.name,
      sessionId: e.sessionId,
      tmuxSessionName: claudeTmuxName(slug, e.name),
      lastActiveMs: e.lastEntryMs,
      // Always false here; `useHarnessSessions` re-annotates against
      // the live tmux name set.
      isLive: false,
      extras: {
        managedName: e.name,
        derivedState: e.state,
        queued: e.queued,
      },
    }));
    return out;
  },

  buildArgs(args: HarnessSpawnArgs) {
    const displayName =
      args.managedName !== null
        ? args.managedName
        : (args.displayLabel ?? "primary");
    return [
      "claude",
      ...wtSessionArgs({
        wtPath: args.wtPath,
        name: args.managedName,
        displayName,
      }),
    ];
  },

  reapState(liveSlugs) {
    reapClaudeNames(liveSlugs);
  },
};

/**
 * Parse a tmux session name and decide whether it represents a Claude
 * session for `slug`. Claude uses `<slug>` for primary and
 * `<slug>~<name>` for named. The `startsWith(`${slug}~`)` guard
 * excludes other harnesses' tmux names (`<slug>-codex`, etc.) and
 * non-AI kinds (`<slug>-diff`, `-shell`, `-action`) — none of those
 * carry a `~` separator, so any name that starts with `<slug>~` is
 * unambiguously a named claude session. `validateSessionName` allows
 * hyphens in the managed portion, so we don't filter on hyphens here.
 */
export function parseClaudeTmuxName(
  name: string,
  slug: string,
): { matches: boolean; managedName: string | null } {
  if (name === slug) return { matches: true, managedName: null };
  if (name.startsWith(`${slug}~`)) {
    return { matches: true, managedName: name.slice(slug.length + 1) };
  }
  return { matches: false, managedName: null };
}

/** Resolve the deterministic UUID for a Claude session on this slug. */
export function claudeSessionId(wtPath: string, managedName: string | null): string {
  return wtSessionUuid(wtPath, managedName);
}

/** Live persisted names for a Claude slug (drop-in for `listClaudeNames`). */
export function listClaudePersistedNames(slug: string): string[] {
  return listClaudeNames(slug);
}
