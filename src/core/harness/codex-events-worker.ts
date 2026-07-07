/**
 * Worker-side Codex activity poller. All synchronous rollout-tree
 * scanning, file stat/read, and JSON parsing lives here so live Codex
 * sessions cannot stall the TUI render/input thread.
 */
import { closeSync, openSync, readSync, statSync } from "node:fs";

import { latestRolloutForCwd } from "./codex.ts";
import type {
  ActiveCodexSlug,
  CodexEventsWorkerEvent,
  CodexEventsWorkerMessage,
  CodexEventsWorkerResult,
} from "./codex-events-protocol.ts";

declare var self: Worker;

/** Per-slug polling state. */
type SlugState = {
  /** Path of the rollout file being tailed. */
  path: string;
  /** Byte offset up to which we have already processed. */
  offset: number;
  /** File mtime at last check — used to detect rollout rotation. */
  mtimeMs: number;
};

// Maximum bytes to read per tick per slot. Keeps a burst of rapid events
// from flooding the activity pane in a single frame.
const MAX_READ_PER_TICK = 32 * 1024;

const slugState = new Map<string, SlugState>();

function post(msg: CodexEventsWorkerResult): void {
  postMessage(msg);
}

self.onmessage = (event: MessageEvent<CodexEventsWorkerMessage>) => {
  const msg = event.data;
  if (msg.type === "stop") {
    slugState.clear();
    return;
  }
  try {
    const events = poll(msg.active);
    if (events.length > 0) post({ type: "events", events });
  } catch (err) {
    post({ type: "warn", message: err instanceof Error ? err.message : String(err) });
  }
};

function poll(active: readonly ActiveCodexSlug[]): CodexEventsWorkerEvent[] {
  const activeSet = new Set(active.map((a) => a.slug));
  for (const slug of slugState.keys()) {
    if (!activeSet.has(slug)) slugState.delete(slug);
  }
  const events: CodexEventsWorkerEvent[] = [];
  for (const { slug, wtPath } of active) {
    try {
      pollSlug(slug, wtPath, slugState, events);
    } catch {
      // Per-slug poll failures are expected during rollout rotation or
      // concurrent writes. Drop this tick and let the next poll recover.
    }
  }
  return events;
}

function pollSlug(
  slug: string,
  wtPath: string,
  stateBySlug: Map<string, SlugState>,
  events: CodexEventsWorkerEvent[],
): void {
  const rollout = latestRolloutForCwd(wtPath);
  if (!rollout) return;

  const state = stateBySlug.get(slug);

  // First tick for this slug, or the rollout file changed (rotation).
  // Establish baseline at end-of-file to skip historical events.
  if (!state || state.path !== rollout.path) {
    stateBySlug.set(slug, {
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
  // pick up its rest on the next tick.
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
    emitEvent(obj, slug, events);
  }
}

function push(
  events: CodexEventsWorkerEvent[],
  level: CodexEventsWorkerEvent["level"],
  text: string,
): void {
  events.push({ level, text });
}

function emitEvent(
  obj: Record<string, unknown>,
  slug: string,
  events: CodexEventsWorkerEvent[],
): void {
  const type = obj.type;
  if (type === "event_msg") {
    const payload = obj.payload;
    if (typeof payload !== "object" || payload === null) return;
    const p = payload as Record<string, unknown>;
    const ptype = p.type as string | undefined;

    switch (ptype) {
      case "task_started": {
        push(events, "info", `turn started · ${slug}`);
        break;
      }
      case "task_complete": {
        const ms = p.duration_ms;
        const dur = typeof ms === "number" ? `${ms}ms` : "?ms";
        push(events, "ok", `turn done in ${dur} · ${slug}`);
        break;
      }
      case "turn_aborted": {
        push(events, "warn", `turn interrupted · ${slug}`);
        break;
      }
      case "user_message": {
        const msg = p.message;
        if (typeof msg === "string" && msg.length > 0) {
          const preview = msg.length > 60 ? `${msg.slice(0, 60)}…` : msg;
          push(events, "dim", `-> ${preview} · ${slug}`);
        }
        break;
      }
      case "mcp_tool_call_end": {
        const inv = p.invocation as Record<string, unknown> | null | undefined;
        if (inv && typeof inv === "object") {
          const server = inv.server ?? "?";
          const tool = inv.tool ?? "?";
          push(events, "info", `mcp: ${server}.${tool} · ${slug}`);
        }
        break;
      }
      case "web_search_end": {
        const query = p.query;
        if (typeof query === "string") {
          const preview = query.length > 60 ? `${query.slice(0, 60)}…` : query;
          push(events, "info", `web: ${preview} · ${slug}`);
        }
        break;
      }
      case "token_count": {
        const rl = p.rate_limits as Record<string, unknown> | null | undefined;
        if (rl && typeof rl === "object") {
          const reached = rl.rate_limit_reached_type;
          if (reached != null) push(events, "warn", `rate limit hit · ${slug}`);
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
        push(events, "info", `exec: ${preview} · ${slug}`);
      }
    }
    // Skip function_call_output, message, reasoning items — too noisy.
  }
}
