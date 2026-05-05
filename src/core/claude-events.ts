/**
 * Shared event-to-line converters used by both the `claude -p` action
 * runner (`core/actions.ts`, stream-json from a child process) and the
 * interactive session tailer (`core/session-tail.ts`, jsonl appended to
 * disk by claude).
 *
 * The two formats differ at the envelope level — action stream-json has
 * `result` events carrying duration/token totals, session jsonl wraps
 * each entry with a server-side timestamp and parent UUID metadata —
 * but the inner `message.content` array is the same shape. Everything
 * at and below `messageToLines` is therefore shared; envelope-specific
 * logic stays in the caller.
 */

export type ActionLineKind =
  | "info" // synthesized — start, kill, error, truncation hints
  | "user" // user-typed prompt (session only)
  | "assistant" // assistant text
  | "tool" // tool_use call
  | "tool-result" // tool result (success or error)
  | "stdout" // shell action stdout line
  | "stderr" // shell action stderr line
  | "exit-success" // result event w/ subtype success
  | "exit-failure"; // result event w/ is_error or non-zero exit

export type ActionLine = {
  ts: number;
  kind: ActionLineKind;
  text: string;
};

/**
 * Per-run map of `tool_use_id → call metadata`. Caller-owned so durations
 * stay correct across batched and restarted reads. `messageToLines`
 * mutates: `tool_use` blocks insert, `tool_result` blocks delete.
 */
export type ToolStartMap = Map<string, { toolName: string; ts: number }>;

/**
 * Per-buffer line cap shared by both the action runner and the session
 * tailer. Old lines drop off the front; tail-clip in the viewer is
 * independent. Co-located so tuning one tunes the other.
 */
export const MAX_BUFFERED_LINES = 1000;
/** Cap message-fragment count from a single assistant turn so a runaway response can't blow line buffers. */
const MAX_PIECES_PER_MESSAGE = 50;
/** Brief tool-input truncation length. Keeps each ⚒ row to one TUI line. */
const TOOL_INPUT_BRIEF = 60;
/** User-prompt truncation length. The `> …` row is informational, not the full prompt. */
const PROMPT_BRIEF = 200;

export function asObj(v: unknown): Record<string, unknown> | null {
  return v && typeof v === "object" && !Array.isArray(v)
    ? (v as Record<string, unknown>)
    : null;
}

export function asArr(v: unknown): unknown[] | null {
  return Array.isArray(v) ? v : null;
}

export function formatDuration(ms: number): string {
  if (ms < 1000) return `${ms}ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)}s`;
  const m = Math.floor(ms / 60_000);
  const s = Math.floor((ms % 60_000) / 1000);
  return `${m}m${s.toString().padStart(2, "0")}s`;
}

export function formatTokens(n: number): string {
  if (n < 1000) return `${n}`;
  if (n < 1_000_000) return `${(n / 1000).toFixed(1)}k`;
  return `${(n / 1_000_000).toFixed(1)}M`;
}

/**
 * One-line summary of a tool's input. Tries common keys (command, path,
 * pattern, …) and falls back to empty when nothing useful is on top.
 * Trailing-truncate to TOOL_INPUT_BRIEF; ⚒ rows have to stay one line.
 */
export function briefToolInput(input: unknown): string {
  const obj = asObj(input);
  if (!obj) return "";
  const keys = [
    "command",
    "file_path",
    "path",
    "pattern",
    "query",
    "url",
    "subagent_type",
    "description",
  ];
  for (const k of keys) {
    const v = obj[k];
    if (typeof v === "string" && v.length > 0) {
      const oneLine = v.replaceAll("\n", " ").replace(/\s+/g, " ");
      return oneLine.length > TOOL_INPUT_BRIEF
        ? `${oneLine.slice(0, TOOL_INPUT_BRIEF - 1)}…`
        : oneLine;
    }
  }
  return "";
}

/**
 * Cap at MAX_PIECES_PER_MESSAGE so a runaway response can't drown out
 * the per-run line buffer; report the dropped count back so the caller
 * can render a "…N more truncated" hint instead of silently swallowing.
 */
export function splitMessage(text: string): {
  pieces: string[];
  truncated: number;
} {
  const all: string[] = [];
  for (const line of text.split("\n")) {
    const trimmed = line.replace(/\s+$/, "");
    if (trimmed.length > 0) all.push(trimmed);
  }
  if (all.length <= MAX_PIECES_PER_MESSAGE) {
    return { pieces: all, truncated: 0 };
  }
  return {
    pieces: all.slice(0, MAX_PIECES_PER_MESSAGE),
    truncated: all.length - MAX_PIECES_PER_MESSAGE,
  };
}

function compactPrompt(text: string): string {
  const oneLine = text.replaceAll("\n", " ").replace(/\s+/g, " ").trim();
  return oneLine.length > PROMPT_BRIEF
    ? `${oneLine.slice(0, PROMPT_BRIEF - 1)}…`
    : oneLine;
}

/**
 * Walk one message envelope's `content` array and emit ActionLine entries.
 * Mutates `toolStarts`: tool_use blocks register start metadata, tool_result
 * blocks consume it (so the `→ ok (1.2s)` duration is the real round-trip).
 *
 * `role` distinguishes the two envelopes that carry content:
 *  - `assistant` → text + tool_use blocks
 *  - `user` → tool_result blocks AND typed prompts (interactive only).
 *    Older session entries store the prompt as a bare string in
 *    `message.content`; newer ones use a `[{type: "text", text: ...}]`
 *    array. Both are accepted.
 *
 * `claude -p` never produces typed-prompt user entries (the prompt is a
 * CLI arg), so the action runner never exercises that code path — it's
 * exclusively for the interactive jsonl tail.
 */
export function messageToLines(opts: {
  role: "assistant" | "user";
  message: unknown;
  ts: number;
  toolStarts: ToolStartMap;
}): ActionLine[] {
  const { role, message, ts, toolStarts } = opts;
  const m = asObj(message);
  const out: ActionLine[] = [];
  if (!m) return out;
  if (role === "user" && typeof m.content === "string") {
    const compacted = compactPrompt(m.content);
    if (compacted) out.push({ ts, kind: "user", text: `> ${compacted}` });
    return out;
  }
  const content = asArr(m.content);
  if (!content) return out;
  for (const block of content) {
    const b = asObj(block);
    if (!b) continue;
    if (role === "assistant") {
      if (b.type === "text" && typeof b.text === "string") {
        const { pieces, truncated } = splitMessage(b.text);
        for (const piece of pieces) {
          out.push({ ts, kind: "assistant", text: `  ${piece}` });
        }
        if (truncated > 0) {
          out.push({
            ts,
            kind: "info",
            text: `  …${truncated} more line${truncated === 1 ? "" : "s"} truncated`,
          });
        }
      } else if (b.type === "tool_use") {
        const toolName = typeof b.name === "string" ? b.name : "?";
        const id = typeof b.id === "string" ? b.id : null;
        const arg = briefToolInput(b.input);
        if (id) toolStarts.set(id, { toolName, ts });
        out.push({
          ts,
          kind: "tool",
          text: `  ⚒ ${toolName}${arg ? `(${arg})` : ""}`,
        });
      }
      continue;
    }
    // role === "user"
    if (b.type === "text" && typeof b.text === "string") {
      const compacted = compactPrompt(b.text);
      if (compacted) out.push({ ts, kind: "user", text: `> ${compacted}` });
    } else if (b.type === "tool_result") {
      const id = typeof b.tool_use_id === "string" ? b.tool_use_id : null;
      const start = id ? toolStarts.get(id) : undefined;
      if (id) toolStarts.delete(id);
      const toolName = start?.toolName ?? "?";
      const dur = start ? formatDuration(ts - start.ts) : "—";
      const isErr = b.is_error === true;
      const arrow = isErr ? "✗" : "✓";
      out.push({
        ts,
        kind: "tool-result",
        text: `  ${arrow} ${toolName} → ${isErr ? "err" : "ok"} (${dur})`,
      });
    }
  }
  return out;
}
