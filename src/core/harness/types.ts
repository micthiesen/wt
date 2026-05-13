/**
 * AI coding harness abstraction. wt supports running multiple harnesses
 * (Claude Code, Codex, OpenCode) concurrently per worktree. Each impl
 * lives in its own file under `core/harness/` and registers via
 * `index.ts`.
 *
 * The interface is deliberately narrow — list, spawn, kill, name — so
 * adding a fourth harness is mostly mechanical. Features Claude has
 * but others don't (busy/idle registry, last-prompt summaries) are
 * NOT in the contract; the consumer renders "Unavailable" when those
 * extras are missing. The `extras` field exposes the optional Claude-
 * only data without bloating the core interface.
 */
import type { DerivedState } from "../claude-status.ts";

export type HarnessId = "claude" | "codex" | "opencode";

/**
 * Optional extras only Claude can fill in today. Other harnesses
 * return `null` everywhere here. Keeping them out of the required
 * shape means Codex/OpenCode impls don't need to fake values.
 */
export type HarnessExtras = {
  /**
   * Wt-managed name for the session. Null = primary (Claude only).
   * Codex / OpenCode have no "primary" concept — every session has a
   * model-given id, so this is always null for them.
   */
  managedName: string | null;
  /**
   * Derived state for the per-session status dot. Claude derives this
   * from jsonl tail + tmux liveness + the on-disk `~/.claude/sessions`
   * registry. Others return null and the renderer falls back to a
   * simple live/dead indicator.
   */
  derivedState: DerivedState | null;
  /** Pending-prompt count for the queued badge. Claude-only. */
  queued: number;
};

export type HarnessSession = {
  /** Display name shown in pickers and rows. */
  displayName: string;
  /**
   * Stable handle to resume this exact session. Format is harness-
   * specific (UUID for Claude, rollout id for Codex, `ses_…` for
   * OpenCode). Pass back as `resumeSessionId` to `buildArgs`.
   */
  sessionId: string;
  /**
   * Tmux session name that would currently host this session. The
   * consumer cross-references against the live tmux name set to derive
   * `isLive` — see `useHarnessSessions`. Claude returns the legacy
   * `<slug>` / `<slug>~<name>` format; Codex / OpenCode return
   * `<slug>-codex` / `<slug>-opencode` (single tmux slot per slug per
   * harness for v1).
   */
  tmuxSessionName: string;
  /** Last meaningful activity ms-since-epoch, or null if unknown. */
  lastActiveMs: number | null;
  /** True when a tmux session is currently running this. */
  isLive: boolean;
  extras: HarnessExtras;
};

export type HarnessSpawnArgs = {
  wtPath: string;
  /**
   * Wt-managed name (Claude only). For others, ignored — they generate
   * their own session ids on spawn.
   */
  managedName: string | null;
  /**
   * Resume an existing session, or null to spawn fresh.
   */
  resumeSessionId: string | null;
  /**
   * Optional display label for Claude's `/resume` picker (primary
   * session only). Ignored by other harnesses.
   */
  displayLabel?: string;
};

export interface Harness {
  readonly id: HarnessId;
  readonly label: string;
  /** Sub-affordance letter in the sessions picker. */
  readonly letter: string;
  /** Nerd-Font glyph rendered next to entries. */
  readonly glyph: string;
  /** Theme color hex. */
  readonly color: string;

  /**
   * Tmux session name for a (slug, managedName). Each impl encodes its
   * own scheme so harnesses can coexist on the same slug without
   * colliding. Claude preserves the legacy `<slug>` / `<slug>~<name>`
   * format; Codex/OpenCode use `<slug>-<id>` / `<slug>-<id>~<name>`.
   */
  tmuxSessionName(slug: string, managedName: string | null): string;

  /**
   * Discover every session this harness knows about for the given
   * worktree — live (tmux) and dead (on-disk only). Caller passes the
   * live tmux name set so impls can flag liveness with one pass.
   */
  discoverSessions(opts: {
    slug: string;
    wtPath: string;
    liveTmuxNames: ReadonlySet<string>;
  }): Promise<HarnessSession[]>;

  /** Inner argv to launch (or resume) a session. Spliced into tmux new-session. */
  buildArgs(args: HarnessSpawnArgs): string[];

  /**
   * Reap on-disk state for slugs no longer present. Called at startup.
   * No-op when impl has no on-disk state of its own.
   */
  reapState(liveSlugs: ReadonlySet<string>): void;
}
