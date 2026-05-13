/**
 * Codex activity-pane event poller.
 *
 * Polls the most-recent rollout for each active codex tmux slot and
 * emits one activity-pane line per new event the user cares about.
 * Historical events on startup are skipped — the first tick establishes
 * a byte-offset baseline and only subsequent ticks emit.
 *
 * Call `startCodexEventPolling(getActiveSlugs)` once at TUI startup and
 * pass the returned cleanup to the shutdown path alongside `closeOpencodeDb`.
 */
import { closeSync, openSync, readSync, statSync } from "node:fs";

import { createLogger } from "../logger.ts";
import { latestRolloutForCwd } from "./codex.ts";

const log = createLogger("[codex]");

/** One active codex tmux slot: the wt slug and its cwd. */
export type ActiveCodexSlug = { slug: string; wtPath: string };

/** Per-slug polling state. */
type SlugState = {
  /** Path of the rollout file being tailed. */
  path: string;
  /** Byte offset up to which we have already processed. */
  offset: number;
  /** File mtime at last check — used to detect rollout rotation. */
  mtimeMs: number;
};

const POLL_INTERVAL_MS = 2_500;
// Maximum bytes to read per tick per slot. Keeps a burst of rapid events
// from flooding the activity pane in a single frame.
const MAX_READ_PER_TICK = 32 * 1024;

/**
 * Start polling codex rollouts for active slots.
 *
 * @param getActiveSlugs - Called on every tick; must return the current
 *   list of active codex tmux slots (slug + worktree path). The caller
 *   should keep this cheap (a Map lookup, not a scan).
 * @returns A cleanup function that stops the interval. Call it during
 *   TUI shutdown.
 */
export function startCodexEventPolling(
  getActiveSlugs: () => ReadonlyArray<ActiveCodexSlug>,
): () => void {
  const slugState = new Map<string, SlugState>();

  const handle = setInterval(() => {
    try {
      tick(getActiveSlugs(), slugState);
    } catch (err) {
      log.warn("poll tick failed", { err: String(err) });
    }
  }, POLL_INTERVAL_MS);

  return () => clearInterval(handle);
}

function tick(
  active: ReadonlyArray<ActiveCodexSlug>,
  slugState: Map<string, SlugState>,
): void {
  // Remove state for slugs that are no longer active.
  const activeSet = new Set(active.map((a) => a.slug));
  for (const slug of slugState.keys()) {
    if (!activeSet.has(slug)) slugState.delete(slug);
  }

  for (const { slug, wtPath } of active) {
    try {
      pollSlug(slug, wtPath, slugState);
    } catch (err) {
      log.debug("poll slug error", { slug, err: String(err) });
    }
  }
}

function pollSlug(
  slug: string,
  wtPath: string,
  slugState: Map<string, SlugState>,
): void {
  const rollout = latestRolloutForCwd(wtPath);
  if (!rollout) return;

  const state = slugState.get(slug);

  // First tick for this slug, or the rollout file changed (rotation).
  // Establish baseline at end-of-file to skip historical events.
  if (!state || state.path !== rollout.path) {
    slugState.set(slug, {
      path: rollout.path,
      offset: rollout.size,
      mtimeMs: rollout.mtimeMs,
    });
    return;
  }

  // Nothing changed since last poll.
  if (rollout.mtimeMs === state.mtimeMs) return;

  // Stat the file for its current size.
  let currentSize: number;
  try {
    currentSize = statSync(rollout.path).size;
  } catch {
    return;
  }

  if (currentSize <= state.offset) {
    // File truncated (shouldn't happen with codex, but guard it).
    state.offset = currentSize;
    state.mtimeMs = rollout.mtimeMs;
    return;
  }

  // Read new bytes since last offset.
  const readLen = Math.min(currentSize - state.offset, MAX_READ_PER_TICK);
  let chunk: string;
  try {
    const fd = openSync(rollout.path, "r");
    try {
      const buf = Buffer.alloc(readLen);
      readSync(fd, buf, 0, readLen, state.offset);
      chunk = buf.toString("utf8");
    } finally {
      closeSync(fd);
    }
  } catch {
    return;
  }

  // Advance only by complete lines: if the chunk ends mid-line
  // (because codex was writing concurrently or we hit MAX_READ_PER_TICK
  // mid-event), the trailing partial line stays unconsumed and we'll
  // pick up its rest on the next tick. Previously we advanced by raw
  // `readLen`, which silently dropped any partial last event since the
  // JSON.parse would just fail and the bytes were already skipped.
  const lastNewlineIdx = chunk.lastIndexOf("\n");
  if (lastNewlineIdx === -1) {
    // No complete line in this chunk yet — wait for more.
    state.mtimeMs = rollout.mtimeMs;
    return;
  }
  const consumedBytes = Buffer.byteLength(
    chunk.slice(0, lastNewlineIdx + 1),
    "utf8",
  );
  state.offset += consumedBytes;
  state.mtimeMs = rollout.mtimeMs;

  // Parse and emit complete lines only.
  const lines = chunk.slice(0, lastNewlineIdx).split("\n");
  for (const line of lines) {
    if (!line.trim()) continue;
    let obj: Record<string, unknown>;
    try {
      obj = JSON.parse(line) as Record<string, unknown>;
    } catch {
      continue;
    }
    emitEvent(obj, slug);
  }
}

function emitEvent(obj: Record<string, unknown>, slug: string): void {
  const type = obj.type;
  if (type === "event_msg") {
    const payload = obj.payload;
    if (typeof payload !== "object" || payload === null) return;
    const p = payload as Record<string, unknown>;
    const ptype = p.type as string | undefined;

    switch (ptype) {
      case "task_started": {
        log.event.info(`turn started · ${slug}`);
        break;
      }
      case "task_complete": {
        const ms = p.duration_ms;
        const dur = typeof ms === "number" ? `${ms}ms` : "?ms";
        log.event.ok(`turn done in ${dur} · ${slug}`);
        break;
      }
      case "turn_aborted": {
        log.event.warn(`turn interrupted · ${slug}`);
        break;
      }
      case "user_message": {
        const msg = p.message;
        if (typeof msg === "string" && msg.length > 0) {
          const preview = msg.length > 60 ? `${msg.slice(0, 60)}…` : msg;
          log.event.dim(`-> ${preview} · ${slug}`);
        }
        break;
      }
      case "mcp_tool_call_end": {
        const inv = p.invocation as Record<string, unknown> | null | undefined;
        if (inv && typeof inv === "object") {
          const server = inv.server ?? "?";
          const tool = inv.tool ?? "?";
          log.event.info(`mcp: ${server}.${tool} · ${slug}`);
        }
        break;
      }
      case "web_search_end": {
        const query = p.query;
        if (typeof query === "string") {
          const preview = query.length > 60 ? `${query.slice(0, 60)}…` : query;
          log.event.info(`web: ${preview} · ${slug}`);
        }
        break;
      }
      case "token_count": {
        const rl = p.rate_limits as Record<string, unknown> | null | undefined;
        if (rl && typeof rl === "object") {
          const reached = rl.rate_limit_reached_type;
          if (reached != null) {
            log.event.warn(`rate limit hit · ${slug}`);
          }
        }
        break;
      }
      // Skip agent_message, agent_reasoning, and other noisy subtypes.
      default:
        break;
    }
  } else if (type === "response_item") {
    const payload = obj.payload;
    if (typeof payload !== "object" || payload === null) return;
    const p = payload as Record<string, unknown>;
    if (p.type === "function_call" && p.name === "exec_command") {
      // Extract the `cmd` field from the arguments JSON string.
      const argsStr = p.arguments;
      if (typeof argsStr === "string") {
        let cmd = "<command>";
        try {
          const args = JSON.parse(argsStr) as Record<string, unknown>;
          const raw = args.cmd ?? args.command ?? argsStr;
          cmd = typeof raw === "string" ? raw : argsStr;
        } catch {
          cmd = argsStr;
        }
        const preview = cmd.length > 60 ? `${cmd.slice(0, 60)}…` : cmd;
        log.event.info(`exec: ${preview} · ${slug}`);
      }
    }
    // Skip function_call_output, message, reasoning items — too noisy.
  }
}
