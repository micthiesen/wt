import { Effect } from "effect";
import { run, type RunOptions, type RunResult, type ProcError } from "./proc.ts";

/** Task content is data. Substitution never interprets shell metacharacters. */
export function issueReaderArgs(command: readonly string[], id: string): string[] {
  return command.map((arg) => arg.replaceAll("{id}", id));
}

type ReaderRunner = (argv: readonly string[], options: RunOptions) => Effect.Effect<RunResult, ProcError>;

/** Capture both channels even on failure so partial downloads are not hidden. */
export const readTrackerIssue = Effect.fn("readTrackerIssue")(function* (
  command: readonly string[],
  id: string,
  cwd: string,
  runner: ReaderRunner = run,
) {
  return yield* runner(issueReaderArgs(command, id), { cwd, timeoutMs: 300_000 });
});
