import type { EventKind } from "../../logger.ts";

/** One active codex tmux slot: the wt slug and its cwd. */
export type ActiveCodexSlug = { slug: string; wtPath: string };

export type CodexEventsWorkerMessage =
  | { type: "poll"; active: readonly ActiveCodexSlug[] }
  | { type: "stop" };

export type CodexEventsWorkerEvent = {
  level: EventKind;
  text: string;
};

export type CodexEventsWorkerResult =
  | { type: "events"; events: CodexEventsWorkerEvent[] }
  | { type: "warn"; message: string };
