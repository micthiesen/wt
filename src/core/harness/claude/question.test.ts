import { describe, expect, test } from "bun:test";

import {
  describePendingToolUse,
  type Entry,
  extractPendingContext,
} from "./jsonl.ts";

/** Builds a synthetic assistant `Entry` with the given content blocks — matches the shape `extractPendingContext` walks (`raw.message.content[]`). */
function assistantEntry(content: unknown[]): Entry {
  return { type: "assistant", raw: { message: { content } } };
}

describe("describePendingToolUse", () => {
  test("ExitPlanMode: multi-line plan, heading stripped from the first non-empty line", () => {
    const input = {
      plan: "# My Plan\n\nDo the thing\nAnd then some more",
    };
    expect(describePendingToolUse("ExitPlanMode", input)).toBe(
      "approve plan: My Plan",
    );
  });

  test("ExitPlanMode: nested heading marker (##) also stripped", () => {
    expect(
      describePendingToolUse("ExitPlanMode", { plan: "## Step one\nDetails" }),
    ).toBe("approve plan: Step one");
  });

  test("ExitPlanMode: missing plan falls back to bare 'approve plan'", () => {
    expect(describePendingToolUse("ExitPlanMode", {})).toBe("approve plan");
    expect(describePendingToolUse("ExitPlanMode", null)).toBe("approve plan");
  });

  test("ExitPlanMode: blank-only plan also falls back to bare 'approve plan'", () => {
    expect(describePendingToolUse("ExitPlanMode", { plan: "\n\n  \n" })).toBe(
      "approve plan",
    );
  });

  test("AskUserQuestion: questions[0].question shape", () => {
    const input = {
      questions: [{ question: "Should we deploy to prod now?" }],
    };
    expect(describePendingToolUse("AskUserQuestion", input)).toBe(
      "question: Should we deploy to prod now?",
    );
  });

  test("AskUserQuestion: bare question shape", () => {
    expect(
      describePendingToolUse("AskUserQuestion", { question: "Pick a color" }),
    ).toBe("question: Pick a color");
  });

  test("AskUserQuestion: whitespace/newlines in the question collapse to single spaces", () => {
    const input = { question: "Line one\n  Line two\ttabbed" };
    expect(describePendingToolUse("AskUserQuestion", input)).toBe(
      "question: Line one Line two tabbed",
    );
  });

  test("AskUserQuestion: neither shape present falls back to bare 'question'", () => {
    expect(describePendingToolUse("AskUserQuestion", {})).toBe("question");
    expect(describePendingToolUse("AskUserQuestion", { questions: [] })).toBe(
      "question",
    );
  });

  test("generic tool: command key surfaces via briefToolInput", () => {
    expect(
      describePendingToolUse("Bash", { command: "npm test" }),
    ).toBe("allow Bash: npm test");
  });

  test("generic tool: empty input drops the trailing colon", () => {
    expect(describePendingToolUse("SomeTool", {})).toBe("allow SomeTool");
    expect(describePendingToolUse("SomeTool", null)).toBe("allow SomeTool");
  });

  test("ANSI escapes and raw control bytes are scrubbed from the output", () => {
    // \x1b[31m is a CSI color escape, \x07 is a bare BEL control byte —
    // both are terminal-escape-injection vectors if agent-influenced
    // text like a plan or question reaches the TUI unscrubbed.
    const plan = describePendingToolUse("ExitPlanMode", {
      plan: "\x1b[31mDo the thing\x07",
    });
    expect(plan).toBe("approve plan: Do the thing");
    expect(plan).not.toMatch(/[\x1b\x07]/);

    const question = describePendingToolUse("AskUserQuestion", {
      question: "\x1b[31mDeploy?\x07",
    });
    expect(question).toBe("question: Deploy?");
    expect(question).not.toMatch(/[\x1b\x07]/);

    const generic = describePendingToolUse("Bash", {
      command: "\x1b[31mnpm test\x07",
    });
    expect(generic).toBe("allow Bash: npm test");
    expect(generic).not.toMatch(/[\x1b\x07]/);
  });
});

describe("extractPendingContext", () => {
  test("no entries: both fields null", () => {
    expect(extractPendingContext([])).toEqual({
      lastToolUse: null,
      lastAssistantText: null,
    });
  });

  test("single assistant entry: last tool_use block wins over earlier ones in the same entry", () => {
    const entries: Entry[] = [
      assistantEntry([
        { type: "tool_use", name: "Read", input: { file_path: "/a" } },
        { type: "tool_use", name: "Bash", input: { command: "ls" } },
      ]),
    ];
    const ctx = extractPendingContext(entries);
    expect(ctx.lastToolUse).toEqual({
      name: "Bash",
      input: { command: "ls" },
    });
  });

  test("multiple assistant entries: the last entry's tool_use wins over earlier entries", () => {
    const entries: Entry[] = [
      assistantEntry([{ type: "tool_use", name: "Read", input: {} }]),
      { type: "user", raw: {} },
      assistantEntry([
        { type: "tool_use", name: "ExitPlanMode", input: { plan: "Ship it" } },
      ]),
    ];
    const ctx = extractPendingContext(entries);
    expect(ctx.lastToolUse).toEqual({
      name: "ExitPlanMode",
      input: { plan: "Ship it" },
    });
  });

  test("text blocks: most recent non-empty first line wins, blank text blocks don't clobber it", () => {
    const entries: Entry[] = [
      assistantEntry([{ type: "text", text: "First response.\nmore" }]),
      assistantEntry([{ type: "text", text: "Second response, final." }]),
      assistantEntry([{ type: "text", text: "\n  \n" }]),
    ];
    const ctx = extractPendingContext(entries);
    expect(ctx.lastAssistantText).toBe("Second response, final.");
  });

  test("mixed text + tool_use blocks in one entry: both fields populated independently", () => {
    const entries: Entry[] = [
      assistantEntry([
        { type: "text", text: "Let me check that." },
        { type: "tool_use", name: "Grep", input: { pattern: "TODO" } },
      ]),
    ];
    const ctx = extractPendingContext(entries);
    expect(ctx.lastAssistantText).toBe("Let me check that.");
    expect(ctx.lastToolUse).toEqual({
      name: "Grep",
      input: { pattern: "TODO" },
    });
  });

  test("non-assistant entries are ignored", () => {
    const entries: Entry[] = [
      { type: "user", raw: { message: { content: [{ type: "text", text: "hi" }] } } },
      { type: "queue-operation", raw: { operation: "enqueue" } },
    ];
    expect(extractPendingContext(entries)).toEqual({
      lastToolUse: null,
      lastAssistantText: null,
    });
  });
});
