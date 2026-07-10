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
import type { DerivedState } from "./status.ts";

export type HarnessId = "claude" | "codex" | "opencode";

/**
 * Optional extras only Claude can fill in today. Other harnesses
 * return `null` everywhere here. Keeping them out of the required
 * shape means Codex/OpenCode impls don't need to fake values.
 */
export type HarnessExtras = {
  /**
   * Wt-managed name used for tmux/session identity where supported.
   * Null = primary for Claude; Codex keeps its friendly name in a
   * separate ID-to-name registry because its tmux slot is shared.
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
  /**
   * What claude is blocked on when `derivedState === "asking"` (e.g.
   * "permission prompt"), straight from the registry's `waitingFor`.
   * Null in every other state and for non-Claude harnesses.
   */
  waitingFor?: string | null;
  /**
   * Registry `updatedAt` — ms-since-epoch of the session's last status
   * write. For idle/asking/waiting/shell, CC writes once on entering the
   * state, so this is effectively when the session entered its current
   * state; for busy a slow heartbeat keeps it fresh. The claude row
   * renders `now - statusSince` as time-in-state. Claude-only and live-
   * only — null when there's no registry entry (e.g. a dead session),
   * so the row falls back to the jsonl `lastActiveMs`.
   */
  statusSince?: number | null;
  /**
   * Timestamp (ms-since-epoch) of the last message row seen, used by
   * `useHarnessSessions` to finalize `derivedState` once liveness is
   * known. OpenCode populates this; Claude / Codex leave it undefined.
   * Timestamp of the latest harness-native event/message. Kept separate
   * from `lastActiveMs` because some stores update session metadata and
   * message rows independently.
   */
  tailEndedAt?: number | null;
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
   * True when the harness uses a single shared tmux slot per slug
   * (`<slug>-<id>`), so resuming a specific session must displace
   * whatever's running in the slot (`freshSlot`), and only one
   * discovered session can be live at a time. False for claude, which
   * gets a unique tmux name per managed session. This is the capability
   * that used to be spelled `id === "codex" || id === "opencode"` at
   * every call site.
   */
  readonly singleSlot: boolean;
  /**
   * Prefix this harness uses to invoke named skills / slash commands in
   * a prompt. Claude Code uses `/`; OpenCode and Codex use `$`.
   * Substituted into action prompts as `{{skill_prefix}}` at launch
   * time (see `buildActionVars` in `tui/app-helpers.ts`), so a single prompt
   * like `{{skill_prefix}}restack` lands correctly regardless of which
   * harness is the row's primary. Headless prompt actions use the
   * selected primary harness's non-interactive CLI, so they use the
   * same prefix as session-injected prompts.
   */
  readonly skillPrefix: string;
  /**
   * tmux `send-keys` key sequence submitted after a bracketed-paste
   * inject (see `injectIntoSession` in `core/tmux.ts`) to commit the
   * pasted prompt. Most harnesses take a single `Enter`; Claude Code
   * and Codex receive the bracketed paste as a multi-line input blob
   * whose first `Enter` only exits that state, so they need a second
   * to actually submit. Keys are sent in order with a small gap
   * between each. Override per harness when a different sequence
   * (e.g. `C-d`, `C-j`) turns out to fit better.
   */
  readonly injectSubmitKeys: readonly string[];

  /**
   * Tmux session name for a (slug, managedName). Each impl encodes its
   * own scheme so harnesses can coexist on the same slug without
   * colliding. Claude preserves the legacy `<slug>` / `<slug>~<name>`
   * format; Codex/OpenCode use `<slug>-<id>` / `<slug>-<id>~<name>`.
   */
  tmuxSessionName(slug: string, managedName: string | null): string;

  /**
   * Discover every session this harness knows about for the given
   * worktree. Liveness is NOT decided here — the impl returns
   * `isLive: false` for every entry and `useHarnessSessions`
   * re-annotates against the current tmux name set. Decoupling
   * liveness from discovery means the discovery query can cache on
   * `(harnessId, slug)` without invalidating on every 2s tmux poll.
   *
   * Known gap: a session that is live in tmux but absent from the
   * impl's on-disk store (e.g. a hypothetical hand-renamed tmux
   * session, or a spawn whose persistence write failed) won't appear
   * in the picker. The spawn flows persist before attaching, so this
   * is unreachable in practice; flagging here for the future.
   */
  discoverSessions(opts: {
    slug: string;
    wtPath: string;
  }): Promise<HarnessSession[]>;

  /** Inner argv to launch (or resume) a session. Spliced into tmux new-session. */
  buildArgs(args: HarnessSpawnArgs): string[];

  /**
   * Reap on-disk state for slugs no longer present. Called at startup.
   * No-op when impl has no on-disk state of its own.
   */
  reapState(liveSlugs: ReadonlySet<string>): void;
}
