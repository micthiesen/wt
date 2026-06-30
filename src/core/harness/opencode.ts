/**
 * OpenCode harness impl. OpenCode persists sessions in a SQLite
 * database at `~/.local/share/opencode/opencode.db`. The `session`
 * table has one row per session with `directory` carrying the worktree
 * path the user invoked opencode from, so we filter by exact match.
 *
 * Resume: `opencode -s <ses_…>`. Fresh: `opencode` (no args).
 *
 * Tmux session naming: single slot per slug (`<slug>-opencode`) for
 * v1. Switching opencode sessions on the same worktree requires
 * detaching and respawning; multi-tmux-per-slug is a followup.
 *
 * We open the SQLite DB read-only so we never hold a write lock; a
 * read racing an opencode write under DELETE journal would surface as
 * `SQLITE_BUSY` and fall into the `catch` returning []. OpenCode
 * currently uses WAL, which lets concurrent reads succeed without the
 * busy fallback. The handle is module-scoped so repeated
 * discoverSessions calls don't pay the file-open cost.
 */
import { existsSync } from "node:fs";
import { homedir } from "node:os";
import { join } from "node:path";

import { Database } from "bun:sqlite";

import { createLogger } from "../logger.ts";
import type { DerivedState } from "../claude-status.ts";

import type { Harness, HarnessSession, HarnessSpawnArgs } from "./types.ts";

const log = createLogger("[opencode]");

const OPENCODE_GLYPH = "\u{F018D}"; // nf-md-console
const OPENCODE_COLOR = "#a78bfa";
const OPENCODE_TMUX_INFIX = "-opencode";

const OPENCODE_DB = join(
  homedir(),
  ".local",
  "share",
  "opencode",
  "opencode.db",
);

let dbHandle: Database | null = null;

/** Returns the module-scoped read-only DB handle, opening it if needed. */
export function openDb(): Database | null {
  if (dbHandle) return dbHandle;
  if (!existsSync(OPENCODE_DB)) return null;
  try {
    // Read-only so we never contend with opencode's own writer.
    dbHandle = new Database(OPENCODE_DB, { readonly: true });
    return dbHandle;
  } catch (err) {
    log.warn("opencode.db open failed", { err: String(err) });
    return null;
  }
}

/**
 * Prepared statement (created lazily, reused across calls) for querying
 * the latest message for a given session. We pre-prepare once and loop
 * in JS — no per-session statement allocation on hot paths.
 */
let lastMsgStmt: ReturnType<Database["query"]> | null = null;

type LastMsgRow = {
  role: string | null;
  completed: number | null;
  time_updated: number;
};

/**
 * Per-session state cache: keyed on sessionId, value holds the
 * session's last `time_updated` and the derived state + tailEndedAt we
 * computed for it. If `session.time_updated` hasn't changed since the
 * last discover call, we skip the message query and return the cached
 * result — the DB hasn't changed for this session.
 */
type StateCacheEntry = {
  timeUpdated: number;
  derivedState: DerivedState | null;
  /** time_updated of the latest message row, ms-since-epoch. Used by
   * useHarnessSessions to decide abandoned vs. idle when isLive flips. */
  tailEndedAt: number | null;
};
const stateCache = new Map<string, StateCacheEntry>();

/**
 * Derive opencode state from the latest message row, without live info.
 * Liveness re-annotation in `useHarnessSessions` will finalize the
 * state — this produces a "best guess" from DB data alone.
 *
 * Mapping:
 *   role=assistant, completed not null → completed assistant turn → waiting
 *   role=assistant, completed null    → streaming response         → working
 *   role=user                         → model still thinking      → working
 *   no rows                           → nothing happened yet      → idle
 *
 * `waiting` vs `idle` is resolved by liveness in useHarnessSessions.
 * We return `waiting` here so the re-annotator has a concrete state to
 * demote to `idle` (or leave as `waiting`) based on `isLive`.
 */
function deriveOpencodeState(row: LastMsgRow | null): DerivedState | null {
  if (!row) return null; // no messages → caller treats as idle
  if (row.role === "assistant") {
    // completed is null while streaming; non-null means done
    return row.completed == null ? "working" : "waiting";
  }
  if (row.role === "user") {
    // User sent a message; model is (or was) processing it
    return "working";
  }
  return null;
}

export const opencodeHarness: Harness = {
  id: "opencode",
  label: "OpenCode",
  letter: "o",
  glyph: OPENCODE_GLYPH,
  color: OPENCODE_COLOR,
  singleSlot: true,
  // OpenCode skills are invoked with a `$` prefix (e.g. $restack).
  skillPrefix: "$",
  // OpenCode commits a pasted prompt on a single Enter.
  injectSubmitKeys: ["Enter"],

  tmuxSessionName(slug, _managedName) {
    // Single-tmux-per-slug for v1 — same model as codex. The opencode
    // session id is supplied via `-s` to the CLI, not via tmux name.
    return `${slug}${OPENCODE_TMUX_INFIX}`;
  },

  async discoverSessions({ slug, wtPath }) {
    const db = openDb();
    if (!db) return [];
    let rows: Array<{ id: string; title: string; time_updated: number }>;
    try {
      const stmt = db.query<
        { id: string; title: string; time_updated: number },
        { $directory: string }
      >(
        "SELECT id, title, time_updated FROM session WHERE directory = $directory AND time_archived IS NULL ORDER BY time_updated DESC",
      );
      // bun:sqlite expects named-param keys to keep their `$` prefix.
      rows = stmt.all({ $directory: wtPath });
    } catch (err) {
      log.warn("opencode.db query failed", { err: String(err) });
      return [];
    }

    // Prepare the per-session last-message query once; reuse across all
    // sessions in this discover call (and future calls — handle is
    // module-scoped).
    if (!lastMsgStmt) {
      try {
        lastMsgStmt = db.query<LastMsgRow, { $sid: string }>(
          `SELECT json_extract(data,'$.role')           AS role,
                  json_extract(data,'$.time.completed') AS completed,
                  time_updated
           FROM message
           WHERE session_id = $sid
           ORDER BY time_created DESC
           LIMIT 1`,
        );
      } catch (err) {
        log.warn("opencode.db prepare lastMsgStmt failed", { err: String(err) });
      }
    }

    const tmuxName = `${slug}${OPENCODE_TMUX_INFIX}`;
    const out: HarnessSession[] = rows.map((row) => {
      // Check state cache before querying the message table.
      const cached = stateCache.get(row.id);
      let derivedState: DerivedState | null;
      let tailEndedAt: number | null;
      if (cached && cached.timeUpdated === row.time_updated) {
        derivedState = cached.derivedState;
        tailEndedAt = cached.tailEndedAt;
      } else {
        let msgRow: LastMsgRow | null = null;
        if (lastMsgStmt) {
          try {
            msgRow = (lastMsgStmt.get({ $sid: row.id }) as LastMsgRow | null | undefined) ?? null;
          } catch (err) {
            log.warn("opencode.db lastMsg query failed", { err: String(err), sessionId: row.id });
          }
        }
        derivedState = deriveOpencodeState(msgRow);
        tailEndedAt = msgRow?.time_updated ?? null;
        stateCache.set(row.id, { timeUpdated: row.time_updated, derivedState, tailEndedAt });
      }

      return {
        displayName: row.title || row.id.slice(0, 8),
        sessionId: row.id,
        tmuxSessionName: tmuxName,
        // SQLite stores time_updated as ms-since-epoch (per opencode schema).
        lastActiveMs: row.time_updated,
        // `useHarnessSessions` re-annotates against the live tmux set.
        isLive: false,
        extras: {
          managedName: null,
          derivedState,
          queued: 0,
          // Stash tailEndedAt for the re-annotator in useHarnessSessions:
          // used to decide idle vs. abandoned when not live.
          tailEndedAt,
        },
      };
    });
    return out;
  },

  buildArgs(args: HarnessSpawnArgs) {
    if (args.resumeSessionId !== null) {
      return ["opencode", "-s", args.resumeSessionId];
    }
    return ["opencode"];
  },

  reapState(_liveSlugs) {
    // No-op: opencode owns its DB; we don't write to it.
  },
};

/** Recognise an opencode tmux session for a slug. */
export function isOpencodeTmuxName(name: string, slug: string): boolean {
  return name === `${slug}${OPENCODE_TMUX_INFIX}`;
}

/** Close the read-only handle. Called from shutdown so the FD doesn't leak. */
export function closeOpencodeDb(): void {
  if (dbHandle) {
    try {
      dbHandle.close();
    } catch {
      // already closed
    }
    dbHandle = null;
  }
}
