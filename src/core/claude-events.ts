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
  | "thinking" // assistant chain-of-thought (session only)
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
 *
 * `batch` is non-null when this call is part of a multi-tool batch
 * within one assistant message — it points at a state object shared
 * by every entry in the batch so the summary line on the result side
 * (`✓ Bash×3 ok, Read×2 ok (3.4s)`) emits exactly once when the last
 * result lands. Detailed tools (Edit/Write/Task/…) and lone non-
 * detailed calls leave `batch` null and render individually.
 */
export type ToolStartEntry = {
  toolName: string;
  ts: number;
  batch: BatchState | null;
};
export type ToolStartMap = Map<string, ToolStartEntry>;

type BatchState = {
  /** Total duration accumulator across the batch. */
  startedAt: number;
  /** tool_use_ids in this batch that haven't seen their result yet. */
  remaining: number;
  /** Per-tool counters; insertion-ordered so the summary preserves call order. */
  results: Map<
    string,
    { ok: number; err: number; durMs: number }
  >;
};

/**
 * Tools that always render individually, even inside a multi-call
 * message. Edits/writes carry their target path — collapsing them to
 * `Edit×3` would hide which files claude touched. `Task` invokes a
 * subagent and the subagent_type / description is the whole point.
 */
const DETAILED_TOOLS = new Set([
  "Edit",
  "Write",
  "MultiEdit",
  "NotebookEdit",
  "Task",
]);

/**
 * Per-buffer line cap shared by both the action runner and the session
 * tailer. Old lines drop off the front; tail-clip in the viewer is
 * independent. Co-located so tuning one tunes the other.
 */
export const MAX_BUFFERED_LINES = 1000;
/** Cap message-fragment count from a single assistant turn so a runaway response can't blow line buffers. */
const MAX_PIECES_PER_MESSAGE = 50;
/** Compaction length for thinking + queued lines. */
const META_BRIEF = 200;

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
 * Whitespace is collapsed so the row stays single-line; length-capping
 * is OpenTUI's job (`<text wrapMode="none" truncate>` clips at the
 * pane width, so cutting at a fixed 60 chars here just hides characters
 * the pane could have shown).
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
      return v.replaceAll("\n", " ").replace(/\s+/g, " ");
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

type UserTextClass =
  | { kind: "drop" }
  | { kind: "interrupted" }
  | { kind: "prompt"; text: string };

const CMD_MESSAGE_RE =
  /^\s*<command-message>([^<]*)<\/command-message>\s*<command-name>([^<]*)<\/command-name>(?:\s*<command-args>([\s\S]*?)<\/command-args>)?/;

/**
 * Classify user-envelope text into one of: drop (auto-injected noise
 * the user already saw via the matching tool call), interrupted (a
 * cancel marker worth surfacing), or prompt (real user-typed content).
 *
 *  - skill bodies (`Base directory for this skill: …`) and slash-
 *    command bodies (`# /<name>`) are auto-injected after the
 *    matching `⚒ Skill` / `⚒ <Tool>` call; the call already conveys
 *    the invocation, so the body is noise. Drop.
 *  - command-message XML wraps a `/<name> <args>` invocation in
 *    `<command-message><command-name>…<command-args>` tags. Unwrap
 *    to `> /<name> <args>` so the user's actual typed input shows.
 *  - `[Request interrupted by user]` → `! interrupted` info line.
 *
 * Applied to both bare string content and array text blocks so the
 * XML-stripping logic runs in one place.
 */
function classifyUserText(text: string): UserTextClass {
  if (text.startsWith("[Request interrupted")) return { kind: "interrupted" };
  if (text.startsWith("Base directory for this skill:")) return { kind: "drop" };
  const cmd = CMD_MESSAGE_RE.exec(text);
  if (cmd) {
    const name = (cmd[2] ?? "").trim();
    const args = (cmd[3] ?? "").trim();
    const rendered = args ? `${name} ${args}` : name;
    return { kind: "prompt", text: rendered };
  }
  if (/^#\s+\//.test(text)) return { kind: "drop" };
  return { kind: "prompt", text };
}

/**
 * Run `text` through `classifyUserText` and append the appropriate
 * line(s) to `out`. Shared between the bare-string envelope path and
 * the per-text-block path so the XML / interrupt / drop logic only
 * lives in one place.
 *
 * Real prompts use the same newline-split + per-line cap as assistant
 * text, so a multi-paragraph prompt renders as multiple rows instead
 * of collapsing to a single truncated `> …` line. The leading `>` is
 * on the first row only; subsequent rows align with the assistant
 * indent.
 */
function appendUserText(out: ActionLine[], ts: number, text: string): void {
  const cls = classifyUserText(text);
  if (cls.kind === "drop") return;
  if (cls.kind === "interrupted") {
    out.push({ ts, kind: "info", text: "! interrupted" });
    return;
  }
  const { pieces, truncated } = splitMessage(cls.text);
  if (pieces.length === 0) return;
  pieces.forEach((piece, i) => {
    out.push({
      ts,
      kind: "user",
      text: `${i === 0 ? "> " : "  "}${piece}`,
    });
  });
  if (truncated > 0) {
    out.push({
      ts,
      kind: "info",
      text: `  …${truncated} more line${truncated === 1 ? "" : "s"} truncated`,
    });
  }
}

/**
 * Compact a multi-line meta string (thinking text, queued prompt) to
 * a single dim-prefixed line — for content that doesn't earn the
 * multi-row treatment user prompts and assistant text get.
 */
export function compactMeta(text: string): string {
  const oneLine = text.replaceAll("\n", " ").replace(/\s+/g, " ").trim();
  return oneLine.length > META_BRIEF
    ? `${oneLine.slice(0, META_BRIEF - 1)}…`
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
    appendUserText(out, ts, m.content);
    return out;
  }
  const content = asArr(m.content);
  if (!content) return out;
  if (role === "assistant") {
    return assistantToLines(content, ts, toolStarts);
  }
  // role === "user"
  for (const block of content) {
    const b = asObj(block);
    if (!b) continue;
    if (b.type === "text" && typeof b.text === "string") {
      appendUserText(out, ts, b.text);
    } else if (b.type === "tool_result") {
      const id = typeof b.tool_use_id === "string" ? b.tool_use_id : null;
      const start = id ? toolStarts.get(id) : undefined;
      if (id) toolStarts.delete(id);
      const isErr = b.is_error === true;
      const durMs = start ? ts - start.ts : 0;
      // Batched call → accumulate into shared state; emit one
      // summary line when the last result lands. Unbatched (lone or
      // detailed) → legacy per-result line.
      if (start?.batch) {
        const batch = start.batch;
        const r = batch.results.get(start.toolName) ?? {
          ok: 0,
          err: 0,
          durMs: 0,
        };
        if (isErr) r.err++;
        else r.ok++;
        r.durMs += durMs;
        batch.results.set(start.toolName, r);
        batch.remaining--;
        if (batch.remaining === 0) {
          out.push({
            ts,
            kind: "tool-result",
            text: `  ${formatBatchResult(batch)}`,
          });
        }
        continue;
      }
      const toolName = start?.toolName ?? "?";
      const dur = start ? formatDuration(durMs) : "—";
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

/**
 * Render one assistant message's content blocks. text/thinking
 * stream out in source order; tool_use blocks are classified
 * detailed-vs-bulk and rendered with a single summary line
 * (`⚒ Bash×3, Read×2`) at the position of the first bulk call when
 * 2+ bulk tools share the message.
 */
function assistantToLines(
  content: unknown[],
  ts: number,
  toolStarts: ToolStartMap,
): ActionLine[] {
  const out: ActionLine[] = [];
  // Bulk classification needs total count up front, so scan once for
  // bulk tool names + ids before emitting in source order.
  const bulkNames: string[] = [];
  const bulkIds: string[] = [];
  let firstBulkPos: number | null = null;
  content.forEach((block, i) => {
    const b = asObj(block);
    if (!b || b.type !== "tool_use") return;
    const name = typeof b.name === "string" ? b.name : "?";
    if (DETAILED_TOOLS.has(name)) return;
    bulkNames.push(name);
    if (typeof b.id === "string") bulkIds.push(b.id);
    if (firstBulkPos === null) firstBulkPos = i;
  });
  // ≥2 bulk calls share a batch state and emit one summary line at
  // the first bulk position; a lone bulk call just renders inline.
  const useBatch = bulkNames.length >= 2;
  const batch: BatchState | null = useBatch
    ? {
        startedAt: ts,
        remaining: bulkIds.length,
        results: new Map(),
      }
    : null;

  content.forEach((block, i) => {
    const b = asObj(block);
    if (!b) return;
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
      return;
    }
    if (b.type === "thinking" && typeof b.thinking === "string") {
      const compacted = compactMeta(b.thinking);
      if (compacted) out.push({ ts, kind: "thinking", text: `· ${compacted}` });
      return;
    }
    if (b.type !== "tool_use") return;
    const toolName = typeof b.name === "string" ? b.name : "?";
    const id = typeof b.id === "string" ? b.id : null;
    if (DETAILED_TOOLS.has(toolName)) {
      const arg = briefToolInput(b.input);
      if (id) toolStarts.set(id, { toolName, ts, batch: null });
      out.push({
        ts,
        kind: "tool",
        text: `  ⚒ ${toolName}${arg ? `(${arg})` : ""}`,
      });
      return;
    }
    // Bulk tool. Either part of a batch (≥2 in this message) or a
    // lone non-detailed call rendered inline.
    if (useBatch && batch) {
      if (id) toolStarts.set(id, { toolName, ts, batch });
      if (i === firstBulkPos) {
        out.push({
          ts,
          kind: "tool",
          text: `  ⚒ ${formatBatchCall(bulkNames)}`,
        });
      }
      return;
    }
    const arg = briefToolInput(b.input);
    if (id) toolStarts.set(id, { toolName, ts, batch: null });
    out.push({
      ts,
      kind: "tool",
      text: `  ⚒ ${toolName}${arg ? `(${arg})` : ""}`,
    });
  });
  return out;
}

/**
 * `Bash×3, Read×2` — join per-tool counts with insertion order
 * preserved so the summary reads in the order claude issued the
 * calls.
 */
function formatBatchCall(names: readonly string[]): string {
  const counts = new Map<string, number>();
  for (const n of names) counts.set(n, (counts.get(n) ?? 0) + 1);
  return [...counts]
    .map(([name, n]) => `${name}×${n}`)
    .join(", ");
}

/**
 * `✓ Bash×2 ok, Read×3 ok, Grep×1 err (3.4s)` — same shape as the
 * call summary, with ok/err breakdown per tool and one combined
 * duration. The leading glyph is `✓` only if every result succeeded;
 * any failure flips it to `✗` so a glance still reads "this batch
 * had a problem".
 */
function formatBatchResult(batch: BatchState): string {
  let totalDurMs = 0;
  let anyErr = false;
  const parts: string[] = [];
  for (const [name, r] of batch.results) {
    totalDurMs += r.durMs;
    const segs: string[] = [];
    if (r.ok > 0) segs.push(`${name}×${r.ok} ok`);
    if (r.err > 0) {
      segs.push(`${name}×${r.err} err`);
      anyErr = true;
    }
    parts.push(segs.join(", "));
  }
  const arrow = anyErr ? "✗" : "✓";
  return `${arrow} ${parts.join(", ")} (${formatDuration(totalDurMs)})`;
}
