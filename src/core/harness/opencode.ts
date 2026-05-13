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

function openDb(): Database | null {
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

export const opencodeHarness: Harness = {
  id: "opencode",
  label: "OpenCode",
  letter: "o",
  glyph: OPENCODE_GLYPH,
  color: OPENCODE_COLOR,

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
    const tmuxName = `${slug}${OPENCODE_TMUX_INFIX}`;
    const out: HarnessSession[] = rows.map((row) => ({
      displayName: row.title || row.id.slice(0, 8),
      sessionId: row.id,
      tmuxSessionName: tmuxName,
      // SQLite stores time_updated as ms-since-epoch (per opencode
      // schema). Use directly.
      lastActiveMs: row.time_updated,
      // `useHarnessSessions` re-annotates against the live tmux set.
      isLive: false,
      extras: { managedName: null, derivedState: null, queued: 0 },
    }));
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
