/**
 * Activity-pane event poller for OpenCode sessions.
 *
 * Polls the OpenCode SQLite DB on an interval, diffs against a
 * per-session snapshot, and emits one `log.event.*` call per
 * observed transition. The first tick establishes baseline (no events
 * emitted) so the pane doesn't flood with backlog on launch.
 *
 * Events surfaced:
 *   - New user message (prompt text, truncated to 60 chars)
 *   - Assistant response completed (with elapsed time)
 *   - New tool dispatch (tool name)
 *   - Tool error (tool name)
 *   - Session title change
 *   - New session created for an active worktree
 *
 * The module reuses the read-only DB handle from `opencode.ts` via
 * the exported `openDb()`. It never opens a second handle.
 */
import type { Statement } from "bun:sqlite";

import { createLogger } from "../logger.ts";
import { openDb } from "./opencode.ts";

const log = createLogger("[opencode]");

const POLL_INTERVAL_MS = 2_500;
/** Max per-session snapshot entries before LRU-trim. See `tick`. */
const SNAPSHOT_CAP = 256;

// ---------- DB row types ----------

type SessionRow = {
  id: string;
  title: string;
  time_updated: number;
};

type MessageRow = {
  id: string;
  role: string | null;
  completed: number | null;
  time_created: number;
};

type PartRow = {
  id: string;
  type: string | null;
  data: string | null;
};

type UserPartRow = {
  text: string | null;
};

// ---------- prepared statement cache ----------

type Stmts = {
  sessionsByDir: Statement<SessionRow, [{ $directory: string }]>;
  latestMsg: Statement<MessageRow, [{ $sid: string }]>;
  parts: Statement<PartRow, [{ $sid: string; $after: number }]>;
  userText: Statement<UserPartRow, [{ $mid: string }]>;
};

let stmts: Stmts | null = null;

function ensureStmts(): Stmts | null {
  if (stmts) return stmts;
  const db = openDb();
  if (!db) return null;
  try {
    stmts = {
      sessionsByDir: db.query<SessionRow, { $directory: string }>(
        "SELECT id, title, time_updated FROM session WHERE directory = $directory AND time_archived IS NULL",
      ) as unknown as Statement<SessionRow, [{ $directory: string }]>,
      latestMsg: db.query<MessageRow, { $sid: string }>(
        `SELECT id,
                json_extract(data,'$.role')           AS role,
                json_extract(data,'$.time.completed') AS completed,
                time_created
         FROM message
         WHERE session_id = $sid
         ORDER BY time_created DESC
         LIMIT 1`,
      ) as unknown as Statement<MessageRow, [{ $sid: string }]>,
      parts: db.query<PartRow, { $sid: string; $after: number }>(
        `SELECT id, json_extract(data,'$.type') AS type, data
         FROM part
         WHERE session_id = $sid AND time_created > $after
         ORDER BY time_created ASC`,
      ) as unknown as Statement<PartRow, [{ $sid: string; $after: number }]>,
      userText: db.query<UserPartRow, { $mid: string }>(
        `SELECT json_extract(data,'$.text') AS text
         FROM part
         WHERE message_id = $mid AND json_extract(data,'$.type') = 'text'
         ORDER BY time_created ASC
         LIMIT 1`,
      ) as unknown as Statement<UserPartRow, [{ $mid: string }]>,
    };
    return stmts;
  } catch (err) {
    log.warn("opencode events: prepare stmts failed", { err: String(err) });
    return null;
  }
}

// ---------- per-session snapshot ----------

type SessionSnapshot = {
  title: string;
  lastMessageId: string | null;
  lastMessageRole: string | null;
  lastCompletedAt: number | null;
  lastMessageCreatedMs: number | null;
  seenPartIds: Set<string>;
};

// Keyed: `${slug}:${sessionId}`
const snapshots = new Map<string, SessionSnapshot>();
// Keyed: slug -> session ids we know about
const seenSessionIds = new Map<string, Set<string>>();
// True after the first full tick (baseline established, no historical events emitted).
let baselineEstablished = false;

// ---------- tick logic ----------

function tick(getActiveSlugs: () => Array<{ slug: string; wtPath: string }>): void {
  const s = ensureStmts();
  if (!s) return;

  const activeSlugs = getActiveSlugs();
  const isBaseline = !baselineEstablished;

  for (const { slug, wtPath } of activeSlugs) {
    try {
      processSlug(s, slug, wtPath, isBaseline);
    } catch (err) {
      log.warn("opencode event poll error", { slug, err: String(err) });
    }
  }

  // Cap `snapshots` to keep memory bounded across long-running wt
  // processes. OpenCode sessions persist in the DB forever (until
  // archived), so each kill-and-respawn cycle adds a new entry that
  // we'd otherwise hold for the lifetime of the process. Drop oldest-
  // inserted entries when we exceed the cap — they'll get re-seeded
  // (without emitting historical events) the next time their session
  // surfaces in the per-slug processing loop. `seenSessionIds` is a
  // set of small strings, not worth capping.
  while (snapshots.size > SNAPSHOT_CAP) {
    const oldest = snapshots.keys().next().value;
    if (oldest === undefined) break;
    snapshots.delete(oldest);
  }

  baselineEstablished = true;
}

function seedSnapshot(s: Stmts, snapKey: string, session: SessionRow): void {
  const msgRow = s.latestMsg.get({ $sid: session.id }) ?? null;
  snapshots.set(snapKey, {
    title: session.title,
    lastMessageId: msgRow?.id ?? null,
    lastMessageRole: msgRow?.role ?? null,
    lastCompletedAt: msgRow?.completed ?? null,
    lastMessageCreatedMs: msgRow?.time_created ?? null,
    seenPartIds: new Set(),
  });
}

function processSlug(
  s: Stmts,
  slug: string,
  wtPath: string,
  isBaseline: boolean,
): void {
  const sessionRows = s.sessionsByDir.all({ $directory: wtPath });

  if (!seenSessionIds.has(slug)) seenSessionIds.set(slug, new Set());
  const knownIds = seenSessionIds.get(slug)!;

  for (const session of sessionRows) {
    const isNew = !knownIds.has(session.id);
    knownIds.add(session.id);

    const snapKey = `${slug}:${session.id}`;

    if (isBaseline) {
      // First tick: record baseline state, emit nothing.
      seedSnapshot(s, snapKey, session);
      continue;
    }

    if (isNew) {
      log.event.info(`new session: ${session.id.slice(0, 8)} · ${slug}`);
      seedSnapshot(s, snapKey, session);
      continue;
    }

    const snap = snapshots.get(snapKey);
    if (!snap) {
      // Can happen if the session appeared between baseline and now but
      // was missed (e.g. the poller was stopped and restarted). Seed.
      seedSnapshot(s, snapKey, session);
      continue;
    }

    // Title change?
    if (session.title && session.title !== snap.title) {
      log.event.dim(`renamed: ${session.title} · ${slug}`);
      snap.title = session.title;
    }

    // Latest message delta.
    const msgRow = s.latestMsg.get({ $sid: session.id }) ?? null;

    if (msgRow) {
      if (msgRow.id !== snap.lastMessageId) {
        // A new message row appeared.
        if (msgRow.role === "user") {
          const textRow = s.userText.get({ $mid: msgRow.id }) ?? null;
          const prompt = textRow?.text ?? "";
          const truncated = prompt.length > 60 ? prompt.slice(0, 60) + "…" : prompt;
          log.event.dim(`→ ${truncated || "(empty)"} · ${slug}`);
        } else if (
          msgRow.role === "assistant" &&
          msgRow.completed != null &&
          snap.lastCompletedAt == null &&
          snap.lastMessageRole === "assistant"
        ) {
          // A streaming response that was in progress just completed.
          emitResponseDone(msgRow.completed, snap.lastMessageCreatedMs, slug);
        }
        snap.lastMessageId = msgRow.id;
        snap.lastMessageRole = msgRow.role;
        snap.lastCompletedAt = msgRow.completed;
        snap.lastMessageCreatedMs = msgRow.time_created;
      } else if (
        // Same message id but streaming just finished (completed was null, now non-null).
        msgRow.role === "assistant" &&
        msgRow.completed != null &&
        snap.lastCompletedAt == null
      ) {
        emitResponseDone(msgRow.completed, snap.lastMessageCreatedMs, slug);
        snap.lastCompletedAt = msgRow.completed;
      }
    }

    // Scan for new tool parts since the last known message creation time.
    const afterMs = snap.lastMessageCreatedMs ?? 0;
    const newParts = s.parts.all({ $sid: session.id, $after: afterMs });

    for (const part of newParts) {
      if (snap.seenPartIds.has(part.id)) continue;
      snap.seenPartIds.add(part.id);

      if (part.type !== "tool" || !part.data) continue;

      let parsed: Record<string, unknown>;
      try {
        parsed = JSON.parse(part.data) as Record<string, unknown>;
      } catch {
        continue;
      }

      const toolName =
        typeof parsed.tool === "string"
          ? parsed.tool
          : typeof parsed.name === "string"
            ? parsed.name
            : null;

      const stateObj = parsed.state as Record<string, unknown> | undefined;
      const status =
        stateObj && typeof stateObj.status === "string" ? stateObj.status : null;

      if (status === "error") {
        log.event.warn(`tool error: ${toolName ?? "unknown"} · ${slug}`);
      } else {
        // Emit on new dispatch (running, or no status = just started).
        log.event.info(`tool: ${toolName ?? "unknown"} · ${slug}`);
      }
    }
  }
}

function emitResponseDone(
  completedMs: number,
  startedMs: number | null,
  slug: string,
): void {
  const elapsedMs = startedMs != null ? completedMs - startedMs : null;
  const elapsedStr =
    elapsedMs != null && elapsedMs > 0
      ? ` (${(elapsedMs / 1000).toFixed(1)}s)`
      : "";
  log.event.ok(`response done${elapsedStr} · ${slug}`);
}

// ---------- public API ----------

/**
 * Start polling the OpenCode DB for activity events. Returns a cleanup
 * function that stops the poll interval.
 *
 * `getActiveSlugs` is called on every tick to get the current list of
 * active worktree slug + wtPath pairs. In the TUI runtime this is
 * backed by the tmux sessions query cache so we only scan worktrees
 * that actually have a live opencode tmux session.
 */
export function startOpencodeEventPolling(
  getActiveSlugs: () => Array<{ slug: string; wtPath: string }>,
): () => void {
  const timer = setInterval(() => {
    try {
      tick(getActiveSlugs);
    } catch (err) {
      log.warn("opencode event tick threw", { err: String(err) });
    }
  }, POLL_INTERVAL_MS);

  return () => {
    clearInterval(timer);
  };
}
