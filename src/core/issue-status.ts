import { Data, Effect, Schema } from "effect";
import { isTrackerIssueId, resolveIssueId } from "./issue-tracker.ts";
import { run, ProcNonZeroExitError, type ProcError, type RunOptions, type RunResult } from "./proc.ts";

export type IssueStatuses = Readonly<Record<string, string>>;

const decodeStatuses = Schema.decodeUnknownSync(Schema.Struct({
  issues: Schema.Array(Schema.Struct({ id: Schema.String, status: Schema.String })),
}));

export class IssueStatusError extends Data.TaggedError("IssueStatusError")<{
  readonly detail: string;
}> {
  override get message(): string { return `issue status reader: ${this.detail}`; }
}

/** Identity is shared with links/actions, including an explicit empty override. */
export function issueStatusIds(rows: readonly { slug: string; issueId?: string | null }[], prefix: string | null = null): string[] {
  return [...new Set(rows.flatMap((row) => {
    const id = resolveIssueId(row.slug, row.issueId);
    return id && isTrackerIssueId(id, prefix) ? [id] : [];
  }))].sort();
}

export function issueStatusArgs(command: readonly string[], ids: readonly string[]): string[] {
  return command.flatMap((arg) => arg === "{ids}" ? [...ids] : [arg]);
}

/** Reject partial responses: missing means unknown, never an empty status. */
export function parseIssueStatuses(stdout: string, ids: readonly string[]): IssueStatuses {
  let parsed: ReturnType<typeof decodeStatuses>;
  try {
    parsed = decodeStatuses(JSON.parse(stdout), { onExcessProperty: "error" });
  } catch (error) {
    throw new IssueStatusError({ detail: `invalid JSON response: ${error instanceof Error ? error.message : String(error)}` });
  }
  const expected = new Set(ids);
  const statuses = new Map<string, string>();
  for (const { id, status } of parsed.issues) {
    if (!expected.has(id)) throw new IssueStatusError({ detail: `unexpected issue ${JSON.stringify(id)}` });
    if (statuses.has(id)) throw new IssueStatusError({ detail: `duplicate issue ${id}` });
    if (!status.trim() || /[\x00-\x1f\x7f]/.test(status)) {
      throw new IssueStatusError({ detail: `invalid status for ${id}: expected a nonempty single-line string` });
    }
    statuses.set(id, status);
  }
  const missing = ids.filter((id) => !statuses.has(id));
  if (missing.length) throw new IssueStatusError({ detail: `missing issues: ${missing.join(", ")}` });
  return Object.fromEntries(statuses);
}

type StatusRunner = (argv: readonly string[], options: RunOptions) => Effect.Effect<RunResult, ProcError>;

export const fetchIssueStatuses = Effect.fn("fetchIssueStatuses")(function* (
  command: readonly string[], ids: readonly string[], cwd: string, runner: StatusRunner = run,
) {
  if (!ids.length) return {};
  const argv = issueStatusArgs(command, ids);
  const result = yield* runner(argv, { cwd, timeoutMs: 30_000 });
  if (result.exitCode !== 0) return yield* new ProcNonZeroExitError({ argv, result });
  return yield* Effect.try({
    try: () => parseIssueStatuses(result.stdout, ids),
    catch: (error) => error instanceof IssueStatusError ? error : new IssueStatusError({ detail: String(error) }),
  });
});
