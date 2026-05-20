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
import { sanitizeLine } from "./proc.ts";

export type ActionLineKind =
  | "info" // synthesized — start, kill, error, truncation hints
  | "user" // user-typed prompt (session only)
  | "assistant" // assistant text
  | "thinking" // assistant chain-of-thought (session only)
  | "tool" // tool_use call awaiting its result (dim)
  | "tool-ok" // tool result, success — patched onto the call line (green)
  | "tool-err" // tool result, failure — patched onto the call line (red)
  | "stdout" // shell action stdout line
  | "stderr" // shell action stderr line
  | "exit-success" // result event w/ subtype success
  | "exit-failure"; // result event w/ is_error or non-zero exit

/**
 * Monotonic per-buffer line identity. The buffer owner (`session-tail`,
 * `actions`) hands out ids via a `nextId()` closure passed into the
 * parser; the parser stores the id of every tool-call line on the
 * matching `ToolStartEntry` so the later `tool_result` can return a
 * patch op that mutates the original call line in place instead of
 * appending a separate `✓ → ok` line. Non-patchable lines still get
 * ids — uniform shape, near-zero cost, future-proof.
 */
export type ActionLine = {
  id: number;
  ts: number;
  kind: ActionLineKind;
  text: string;
};

/**
 * Replace the buffer entry with id `id`. No-op when the id no longer
 * exists in the buffer — e.g. the call was evicted past the
 * `MAX_BUFFERED_LINES` window between the call and its result, so
 * silently swallowing the patch is correct (the user has scrolled
 * past it long ago).
 */
export type LinePatch = {
  id: number;
  line: ActionLine;
};

/**
 * What one envelope's parse emits. `append` lines push onto the
 * buffer; `patch` ops replace existing entries (matched by `id`) so
 * tool_use → tool_result pairs collapse to a single line that turns
 * green/red when it resolves. Most envelopes emit only `append`;
 * patches show up when a user envelope carries `tool_result` blocks
 * for prior assistant tool_uses.
 */
export type MessageEmit = {
  append: ActionLine[];
  patch: LinePatch[];
};

/**
 * Per-run map of `tool_use_id → call metadata`. Caller-owned so durations
 * stay correct across batched and restarted reads. `messageToLines`
 * mutates: `tool_use` blocks insert, `tool_result` blocks delete.
 *
 * `label` is the call line's text body (e.g. `Edit(/path/to/file.ts)`)
 * stashed at insert time so the result handler can rebuild the line
 * (`✓ Edit(/path/to/file.ts) 1.2s`) without re-walking the original
 * input. `lineId` is the buffer id of the call line — `null` for batch
 * members (the batch has one shared line, addressed via `batch.lineId`).
 *
 * `batch` is non-null when this call is part of a multi-tool batch
 * within one assistant message — it points at a state object shared
 * by every entry in the batch so the summary line on the result side
 * (`✓ Bash×3, Read×2 (3.4s)`) emits exactly once when the last
 * result lands. Detailed tools (Edit/Write/Task/…) and lone non-
 * detailed calls leave `batch` null and render individually.
 */
export type ToolStartEntry = {
  toolName: string;
  label: string;
  ts: number;
  batch: BatchState | null;
  lineId: number | null;
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
  /** Buffer id of the batch summary call line — patched on last result. */
  lineId: number;
  /** Pre-formatted "Bash×3, Read×2" call label for result-line rebuild. */
  callLabel: string;
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
/**
 * Claude appends a "(disable recaps in /config)" hint to every
 * `away_summary`. Pure UI clutter — strip it everywhere we render
 * away-summary content (live tail + post-hoc summary scan).
 */
export const AWAY_RECAP_HINT_RE = /\s*\(disable recaps in [^)]+\)\s*$/i;

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
 * Pull a one-line summary out of a `tool_result.content` payload —
 * used when `is_error: true` so the collapsed result line can carry
 * the reason (`✗ Bash(npm test) err: 3 tests failed 1.2s`) instead of
 * just `err`. Content arrives as either a bare string or an array of
 * `{type: "text", text}` blocks; both shapes get the same treatment:
 * first non-empty line after ANSI scrub, length-capped so the line
 * stays single-row.
 */
const TOOL_ERR_MAX = 120;
export function briefToolResultBody(content: unknown): string | null {
  const pick = (text: string): string | null => {
    for (const raw of text.split("\n")) {
      const cleaned = sanitizeLine(raw).trim();
      if (cleaned.length === 0) continue;
      return cleaned.length > TOOL_ERR_MAX
        ? `${cleaned.slice(0, TOOL_ERR_MAX - 1)}…`
        : cleaned;
    }
    return null;
  };
  if (typeof content === "string") return pick(content);
  const arr = asArr(content);
  if (!arr) return null;
  for (const block of arr) {
    const b = asObj(block);
    if (!b) continue;
    if (typeof b.text === "string") {
      const out = pick(b.text);
      if (out) return out;
    }
  }
  return null;
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
  | { kind: "task-notification"; summary: string; event: string | null }
  | { kind: "local-stdout"; text: string }
  | { kind: "prompt"; text: string };

// Slash-command invocations arrive as three sibling tags. Old user-defined
// commands (`/parallel`, `/check`) emit `<command-message>` first; built-in
// claude commands (`/compact`, `/clear`, `/rename`) emit `<command-name>`
// first. Match each tag independently so either ordering renders correctly.
const COMMAND_LEAD_RE = /^\s*<command-(?:name|message)>/;
const COMMAND_NAME_TAG_RE = /<command-name>([\s\S]*?)<\/command-name>/;
const COMMAND_ARGS_TAG_RE = /<command-args>([\s\S]*?)<\/command-args>/;

const TASK_NOTIFICATION_RE = /^\s*<task-notification>/;
const SUMMARY_RE = /<summary>([\s\S]*?)<\/summary>/;
const EVENT_RE = /<event>([\s\S]*?)<\/event>/;

const LOCAL_COMMAND_CAVEAT_RE = /^\s*<local-command-caveat>/;
const LOCAL_COMMAND_STDOUT_RE =
  /^\s*<local-command-stdout>([\s\S]*?)<\/local-command-stdout>\s*$/;

/**
 * Strip the wrapping verbiage Claude Code's Monitor / Background-command
 * emitter adds around the meaningful summary so the row reads as the
 * thing being watched, not the protocol envelope. Examples:
 *
 *  - `Monitor event: "deploy progress and failures"` →
 *    `Monitor — deploy progress and failures`
 *  - `Background command "Run pnpm deploy" completed (exit code 0)` →
 *    `Background — Run pnpm deploy (exit 0)`
 *
 * Falls back to the raw summary verbatim when neither pattern matches.
 */
function compactTaskSummary(raw: string): string {
  const trimmed = raw.trim();
  const monitor = /^Monitor event:\s*"([^"]*)"\s*$/.exec(trimmed);
  if (monitor) return `Monitor — ${monitor[1]}`;
  const bg =
    /^Background command\s*"([^"]*)"\s*completed\s*\(exit code (-?\d+)\)\s*$/.exec(
      trimmed,
    );
  if (bg) return `Background — ${bg[1]} (exit ${bg[2]})`;
  return trimmed;
}

/**
 * Classify user-envelope text into one of: drop (auto-injected noise
 * the user already saw via the matching tool call), interrupted (a
 * cancel marker worth surfacing), task-notification (Monitor /
 * Background-command auto-feedback), local-stdout (slash-command
 * stdout to surface), or prompt (real user-typed content).
 *
 *  - skill bodies (`Base directory for this skill: …`) and slash-
 *    command bodies (`# /<name>`) are auto-injected after the
 *    matching `⚒ Skill` / `⚒ <Tool>` call; the call already conveys
 *    the invocation, so the body is noise. Drop.
 *  - `<command-name>` / `<command-message>` / `<command-args>` siblings
 *    wrap a `/<name> <args>` invocation. Built-in commands emit them in
 *    the reverse order from user-defined commands, so we match each tag
 *    independently and unwrap to `> /<name> <args>`.
 *  - task-notification XML wraps Claude Code's auto-injected Monitor
 *    events and Background-command completions. Surface the summary +
 *    event body as a dim block (one header line, then indented body
 *    rows) instead of letting the raw XML render through the multi-
 *    line user-prompt path.
 *  - `<local-command-caveat>` is auto-injected boilerplate that follows
 *    every local slash command; drop. `<local-command-stdout>` carries
 *    the actual output of the command (e.g. `Compacted (ctrl+o…)`,
 *    `Authentication successful…`); surface it as an info block.
 *  - `[Request interrupted by user]` → `! interrupted` info line.
 *
 * Applied to both bare string content and array text blocks so the
 * XML-stripping logic runs in one place.
 */
function classifyUserText(text: string): UserTextClass {
  if (text.startsWith("[Request interrupted")) return { kind: "interrupted" };
  if (text.startsWith("Base directory for this skill:")) return { kind: "drop" };
  if (TASK_NOTIFICATION_RE.test(text)) {
    const summaryM = SUMMARY_RE.exec(text);
    const summary = summaryM ? compactTaskSummary(summaryM[1] ?? "") : "task";
    const eventM = EVENT_RE.exec(text);
    const event = eventM ? (eventM[1] ?? "").replace(/^\n+|\n+$/g, "") : null;
    return { kind: "task-notification", summary, event: event || null };
  }
  if (LOCAL_COMMAND_CAVEAT_RE.test(text)) return { kind: "drop" };
  const stdoutM = LOCAL_COMMAND_STDOUT_RE.exec(text);
  if (stdoutM) {
    const body = (stdoutM[1] ?? "").trim();
    if (!body) return { kind: "drop" };
    return { kind: "local-stdout", text: body };
  }
  if (COMMAND_LEAD_RE.test(text)) {
    const nameM = COMMAND_NAME_TAG_RE.exec(text);
    if (nameM) {
      const name = (nameM[1] ?? "").trim();
      const argsM = COMMAND_ARGS_TAG_RE.exec(text);
      const args = (argsM?.[1] ?? "").trim();
      const rendered = args ? `${name} ${args}` : name;
      return { kind: "prompt", text: rendered };
    }
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
function appendUserText(
  out: ActionLine[],
  ts: number,
  text: string,
  nextId: () => number,
): void {
  const cls = classifyUserText(text);
  if (cls.kind === "drop") return;
  if (cls.kind === "interrupted") {
    out.push({ id: nextId(), ts, kind: "info", text: "! interrupted" });
    return;
  }
  if (cls.kind === "task-notification") {
    out.push({ id: nextId(), ts, kind: "info", text: `◉ ${cls.summary}` });
    if (cls.event) {
      const { pieces, truncated } = splitMessage(cls.event);
      for (const piece of pieces) {
        out.push({ id: nextId(), ts, kind: "info", text: `  ${piece}` });
      }
      if (truncated > 0) {
        out.push({
          id: nextId(),
          ts,
          kind: "info",
          text: `  …${truncated} more line${truncated === 1 ? "" : "s"} truncated`,
        });
      }
    }
    return;
  }
  if (cls.kind === "local-stdout") {
    // ANSI-stripped per line — claude's local commands emit colour codes
    // (e.g. `/compact` wraps its message in dim attrs) that would render
    // as escape gibberish through the user-prompt path.
    const cleaned = cls.text
      .split("\n")
      .map((piece) => sanitizeLine(piece).trimEnd())
      .filter((piece) => piece.length > 0);
    if (cleaned.length === 0) return;
    cleaned.forEach((piece, i) => {
      out.push({
        id: nextId(),
        ts,
        kind: "info",
        text: `${i === 0 ? "↳ " : "  "}${piece}`,
      });
    });
    return;
  }
  const { pieces, truncated } = splitMessage(cls.text);
  if (pieces.length === 0) return;
  pieces.forEach((piece, i) => {
    out.push({
      id: nextId(),
      ts,
      kind: "user",
      text: `${i === 0 ? "> " : "  "}${piece}`,
    });
  });
  if (truncated > 0) {
    out.push({
      id: nextId(),
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
 * Walk one message envelope's `content` array and emit a delta —
 * `append` lines (new buffer entries) plus `patch` ops (existing call
 * lines mutated in place when their results arrive). Mutates
 * `toolStarts`: tool_use blocks insert, tool_result blocks consume.
 *
 * `role` distinguishes the two envelopes that carry content:
 *  - `assistant` → text + tool_use blocks (always append, never patch).
 *  - `user` → tool_result blocks AND typed prompts (interactive only).
 *    Older session entries store the prompt as a bare string in
 *    `message.content`; newer ones use a `[{type: "text", text: ...}]`
 *    array. Both are accepted. tool_result blocks patch the matching
 *    call line (single-tool) or the batch summary line (batched);
 *    when the call has been evicted past the buffer window the patch
 *    is silently dropped at apply time.
 *
 * `nextId` is the buffer's monotonic id factory — pass the same
 * closure across all `parseEntry` calls for a given buffer so ids
 * stay unique.
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
  nextId: () => number;
}): MessageEmit {
  const { role, message, ts, toolStarts, nextId } = opts;
  const emit: MessageEmit = { append: [], patch: [] };
  const m = asObj(message);
  if (!m) return emit;
  if (role === "user" && typeof m.content === "string") {
    appendUserText(emit.append, ts, m.content, nextId);
    return emit;
  }
  const content = asArr(m.content);
  if (!content) return emit;
  if (role === "assistant") {
    return assistantToEmit(content, ts, toolStarts, nextId);
  }
  // role === "user"
  for (const block of content) {
    const b = asObj(block);
    if (!b) continue;
    if (b.type === "text" && typeof b.text === "string") {
      appendUserText(emit.append, ts, b.text, nextId);
    } else if (b.type === "tool_result") {
      const id = typeof b.tool_use_id === "string" ? b.tool_use_id : null;
      const start = id ? toolStarts.get(id) : undefined;
      if (id) toolStarts.delete(id);
      const isErr = b.is_error === true;
      const durMs = start ? ts - start.ts : 0;
      // Batched call → accumulate into shared state; patch the
      // summary line when the last result lands.
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
          const anyErr = [...batch.results.values()].some((v) => v.err > 0);
          emit.patch.push({
            id: batch.lineId,
            line: {
              id: batch.lineId,
              ts,
              kind: anyErr ? "tool-err" : "tool-ok",
              text: `  ${formatBatchResult(batch)}`,
            },
          });
        }
        continue;
      }
      // Single tool. Patch the call line in place when we still have
      // its id (the live case); fall through to appending a standalone
      // result line when the call was never seen (seed-window orphan).
      const arrow = isErr ? "✗" : "✓";
      const dur = start ? formatDuration(durMs) : "—";
      const errBody = isErr ? briefToolResultBody(b.content) : null;
      if (start && start.lineId !== null) {
        const label = start.label;
        const errPart = errBody ? ` err: ${errBody}` : "";
        const text = `  ${arrow} ${label}${errPart} ${dur}`;
        emit.patch.push({
          id: start.lineId,
          line: {
            id: start.lineId,
            ts,
            kind: isErr ? "tool-err" : "tool-ok",
            text,
          },
        });
        continue;
      }
      // Orphan path: no call line to patch. Synthesize a standalone
      // result so the user still sees that *something* finished.
      const label = start?.label ?? start?.toolName ?? "?";
      const errPart = errBody ? ` err: ${errBody}` : "";
      const orphanId = nextId();
      emit.append.push({
        id: orphanId,
        ts,
        kind: isErr ? "tool-err" : "tool-ok",
        text: `  ${arrow} ${label}${errPart} ${dur}`,
      });
    }
  }
  return emit;
}

/**
 * Render one assistant message's content blocks. text/thinking
 * stream out in source order; tool_use blocks are classified
 * detailed-vs-bulk and rendered with a single summary line
 * (`⚒ Bash×3, Read×2`) at the position of the first bulk call when
 * 2+ bulk tools share the message.
 *
 * Stashes the call line's id + label on each `ToolStartEntry` so the
 * later `tool_result` patches the same line — see `messageToLines`.
 */
function assistantToEmit(
  content: unknown[],
  ts: number,
  toolStarts: ToolStartMap,
  nextId: () => number,
): MessageEmit {
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
  const batchLineId = useBatch ? nextId() : 0;
  const batchLabel = useBatch ? formatBatchCall(bulkNames) : "";
  const batch: BatchState | null = useBatch
    ? {
        startedAt: ts,
        remaining: bulkIds.length,
        results: new Map(),
        lineId: batchLineId,
        callLabel: batchLabel,
      }
    : null;

  content.forEach((block, i) => {
    const b = asObj(block);
    if (!b) return;
    if (b.type === "text" && typeof b.text === "string") {
      const { pieces, truncated } = splitMessage(b.text);
      for (const piece of pieces) {
        out.push({
          id: nextId(),
          ts,
          kind: "assistant",
          text: `  ${piece}`,
        });
      }
      if (truncated > 0) {
        out.push({
          id: nextId(),
          ts,
          kind: "info",
          text: `  …${truncated} more line${truncated === 1 ? "" : "s"} truncated`,
        });
      }
      return;
    }
    if (b.type === "thinking" && typeof b.thinking === "string") {
      const compacted = compactMeta(b.thinking);
      if (compacted) {
        out.push({
          id: nextId(),
          ts,
          kind: "thinking",
          text: `· ${compacted}`,
        });
      }
      return;
    }
    if (b.type !== "tool_use") return;
    const toolName = typeof b.name === "string" ? b.name : "?";
    const useId = typeof b.id === "string" ? b.id : null;
    const arg = briefToolInput(b.input);
    const label = `${toolName}${arg ? `(${arg})` : ""}`;
    if (DETAILED_TOOLS.has(toolName)) {
      const lineId = nextId();
      if (useId) {
        toolStarts.set(useId, { toolName, label, ts, batch: null, lineId });
      }
      out.push({ id: lineId, ts, kind: "tool", text: `  ⚒ ${label}` });
      return;
    }
    // Bulk tool. Either part of a batch (≥2 in this message) or a
    // lone non-detailed call rendered inline.
    if (useBatch && batch) {
      if (useId) {
        // Members share the batch line id via `batch.lineId`; their
        // own `lineId` stays null so the result handler routes them
        // through the batch path rather than trying to patch their
        // own (non-existent) line.
        toolStarts.set(useId, {
          toolName,
          label,
          ts,
          batch,
          lineId: null,
        });
      }
      if (i === firstBulkPos) {
        out.push({
          id: batch.lineId,
          ts,
          kind: "tool",
          text: `  ⚒ ${batch.callLabel}`,
        });
      }
      return;
    }
    const lineId = nextId();
    if (useId) {
      toolStarts.set(useId, { toolName, label, ts, batch: null, lineId });
    }
    out.push({ id: lineId, ts, kind: "tool", text: `  ⚒ ${label}` });
  });
  return { append: out, patch: [] };
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
 * `✓ Bash×3, Read×2 (3.4s)` on the happy path; `✗ Bash×2 ok, Bash×1
 * err, Read×2 ok (3.4s)` when something failed. The all-ok case
 * drops the redundant per-tool `ok` annotations — line color
 * (`tool-ok` = green) carries that signal — but the err-mixed case
 * spells out the breakdown so the user can see how many of which
 * tool failed without opening the pane.
 */
function formatBatchResult(batch: BatchState): string {
  let totalDurMs = 0;
  let anyErr = false;
  for (const r of batch.results.values()) {
    totalDurMs += r.durMs;
    if (r.err > 0) anyErr = true;
  }
  const dur = formatDuration(totalDurMs);
  if (!anyErr) {
    // Reuse the call-side label verbatim ("Bash×3, Read×2") so the
    // resolved line reads as the same call, just resolved.
    return `✓ ${batch.callLabel} (${dur})`;
  }
  const parts: string[] = [];
  for (const [name, r] of batch.results) {
    const segs: string[] = [];
    if (r.ok > 0) segs.push(`${name}×${r.ok} ok`);
    if (r.err > 0) segs.push(`${name}×${r.err} err`);
    parts.push(segs.join(", "));
  }
  return `✗ ${parts.join(", ")} (${dur})`;
}
