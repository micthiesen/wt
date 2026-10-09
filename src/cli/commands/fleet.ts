/**
 * `wt fleet` — the manager's single audit surface: one row per live
 * worktree joining the ASSERTED work status with observable REALITY
 * (live agent session, PR/merge/CI state), plus the recently-removed
 * rows every fleet surface appends. `--json` is the contract; the
 * human table is a convenience.
 *
 * PR reality comes from ONE batched GraphQL round trip through
 * `fetchGithub` (the same machinery the TUI uses) — never per-row gh
 * calls. GitHub computes `mergeable` lazily: `UNKNOWN` is reported as
 * "computing" and never retried here — the caller re-runs after a few
 * seconds if it cares (the query itself is what triggers the compute).
 */
import { branchIsGone, branchIsMerged, revParse } from "../../core/git.ts";
import { config } from "../../core/config.ts";
import {
  fetchGithub,
  hasGh,
  pickPrForWorktree,
  repoSlug,
} from "../../core/github.ts";
import { operationErrors } from "../../core/errors.ts";
import { readRegistry } from "../../core/harness/claude/registry.ts";
import { edgeIsStaleBySha, type MergeEdge } from "../../core/merge-edges.ts";
import { listSessions } from "../../core/tmux.ts";
import type {
  MergeableState,
  MergeStateStatus,
  PullRequest,
  Worktree,
} from "../../core/types.ts";
import { listWorktrees } from "../../core/worktree.ts";
import {
  verifyStepsHeadline,
  workAge,
  isGated,
  owesPostMergeVerification,
  workRecordRank,
  workStateRank,
  type WorkStatusRecord,
} from "../../core/work-status.ts";
import {
  readWtState,
  recentlyRemovedWorktrees,
  removedJsonEntry,
} from "../../core/wtstate.ts";
import { firstUnknownFlag, hasHelpFlag } from "../args.ts";
import { cyan, dim, green, magenta, red, yellow } from "../colors.ts";
import { renderTable } from "../render.ts";
import { Effect } from "effect";

const io = operationErrors("wt fleet");

const USAGE = `usage: wt fleet [--json]

The fleet audit: one row per live worktree joining the asserted work
status (state, note, risk, staleness) with reality — live agent
session (busy / last activity) and PR state (number, draft, merge
state, mergeability, CI rollup) from one batched GitHub query. The
human's manual TUI section rides along as asserted intent (a name
like "Merge after Release" is a merge-order hint; null/— = inbox).
Recently-removed rows (≤48h) are appended so "everything landed" never
reads as "nothing exists".

  --json    machine-readable array. Every row carries kind: filter
            kind == "live" for the worktrees that exist, "merged" /
            "removed" for the history. Live rows carry work / session /
            pr objects (pr is null with a pr_note when GitHub is
            unavailable); removed rows carry pr and archived_at only.
            Same field, same values, on wt ls --json and
            wt status --all --json.

Merge planning reads two more fields on every live row. base is the
effective merge target — the recorded fork base for a stacked
worktree, else [branch] base — never null, the same value wt ls --json
carries. edges are the merge edges touching that slug, in the same
shape as wt edge --json plus stale; an edge rides BOTH of its endpoint
rows, so dedupe on from/to, and ignore stale ones for ordering exactly
as wt edge does.

Merge fields report "computing" while GitHub is still calculating
mergeability (its UNKNOWN state) — re-run after a few seconds; the
query itself is what triggers the computation.`;

const KNOWN_FLAGS = new Set(["--json", "--help", "-h"]);

/**
 * Lowercase GitHub's SCREAMING_CASE for the JSON contract, mapping the
 * lazily-computed `UNKNOWN` to "computing" — but only on OPEN PRs. On
 * terminal PRs GitHub reports UNKNOWN forever (there is nothing left
 * to compute), so the fields are suppressed rather than lying
 * "computing" for eternity.
 */
function mergeField(
  v: MergeableState | MergeStateStatus | null | undefined,
  prState: PullRequest["state"],
): string | null {
  if (!v || prState !== "OPEN") return null;
  return v === "UNKNOWN" ? "computing" : v.toLowerCase();
}

type SessionInfo = {
  alive: boolean;
  busy: boolean | null;
  last_activity: string | null;
};

/**
 * Per-worktree primary-session liveness. The live set covers every harness;
 * richer busy / last_activity detail is currently available only from
 * Claude's process registry. Worktree Claude primaries register under the
 * slug; "primary" and null are the pre-slug-naming forms, still matched so a
 * session started before that change (or by hand, without `--name`) keeps
 * reporting liveness.
 */
export function sessionInfoFor(
  wt: Worktree,
  liveHarnessSlugs: ReadonlySet<string>,
  liveClaudeSlugs: ReadonlySet<string>,
  registry: ReturnType<typeof readRegistry>,
): SessionInfo {
  const alive = liveHarnessSlugs.has(wt.slug);
  if (!alive) return { alive: false, busy: null, last_activity: null };
  if (!liveClaudeSlugs.has(wt.slug)) {
    return { alive: true, busy: null, last_activity: null };
  }
  const match = registry
    .filter(
      (r) =>
        r.cwd === wt.path &&
        (r.name === wt.slug || r.name === "primary" || r.name === null),
    )
    .sort((a, b) => b.updatedAt - a.updatedAt)[0];
  return {
    alive: true,
    busy: match ? match.status === "busy" || match.status === "shell" : null,
    last_activity:
      match && match.updatedAt > 0
        ? new Date(match.updatedAt).toISOString()
        : null,
  };
}

/**
 * The one batched PR fetch, degraded to a note instead of a crash when
 * GitHub is unreachable: no gh / no resolvable repo yields a
 * self-describing note (fetchGithub would silently return empty maps,
 * indistinguishable from "no PRs"), and a thrown fetch (auth, rate
 * limit, network) becomes its first error line. Rows always emit.
 */
const fetchFleetPrs = Effect.fnUntraced(function* (branches: string[]) {
  if (!(yield* hasGh())) {
    return { prs: new Map(), note: "gh CLI not installed — PR data omitted" };
  }
  if (!(yield* repoSlug())) {
    return {
      prs: new Map(),
      note: "GitHub repo unresolvable (gh not authenticated, or no GitHub remote) — PR data omitted",
    };
  }
  const github = yield* fetchGithub(branches);
  return { prs: github.prs, note: null };
}, Effect.catch((error) =>
  Effect.succeed({
    prs: new Map<string, PullRequest>(),
    note: error.message,
  }),
));

type FleetRow = {
  wt: Worktree;
  /** Manual TUI section (human intent — e.g. merge batching); null = inbox. */
  section: string | null;
  work: (WorkStatusRecord & { stale: boolean }) | null;
  /**
   * Has the branch landed? The same three signals the TUI's
   * `rowHasLanded` accepts, and worth the per-row git calls precisely
   * because this command is the manager's primary sense: a squash
   * merge whose PR record this fetch missed would otherwise read as a
   * plain `ready` on a row that has already merged and still owes a
   * deployed-environment check — the exact silent case the field
   * exists to catch.
   */
  landed: boolean;
  session: SessionInfo;
  pr: PullRequest | undefined;
  /**
   * Effective merge target — the recorded fork base, else trunk. Same
   * derivation (and same never-null contract) as `wt ls --json`'s
   * `base`; it was on that surface and not this one, so the manager,
   * whose primary sense this is, could read a real stack as four
   * independent branches and report that no ordering existed.
   */
  base: string;
  /**
   * Merge edges touching this slug, either direction — so an edge
   * appears on both endpoint rows and readers dedupe on from/to.
   * `wt edge --help` has promised that this surface carries them since
   * edges existed; it did not, and absence of an edge is defined to
   * mean "no known constraint", so the omission read as a clean
   * answer rather than a missing field.
   */
  edges: (MergeEdge & { stale: boolean })[];
};

function workCell(row: FleetRow): string {
  if (!row.work) return dim("—");
  // A gated ready is not a ready. The manager reads this column to
  // build a merge order, and it read `ready` off a gated branch twice.
  if (isGated(row.work)) return yellow(`blocked/${row.work.state}`);
  // Landed and still owing a deployed-environment check. Rendered
  // ahead of the state for the same reason as the gate: the state
  // alone says `ready`, and `ready` on a merged row reads as finished.
  if (row.work.verifyAfterMerge && row.landed) {
    return yellow(`unverified/${row.work.state}`);
  }
  const color =
    row.work.state === "needs-human"
      ? red
      : row.work.state === "needs-testing"
        ? yellow
        : row.work.state === "ready"
          ? green
          : row.work.state === "review"
            ? magenta
            : row.work.state === "working"
              ? cyan
              : dim;
  const parts = [color(row.work.state)];
  if (row.work.risk) parts.push(dim(row.work.risk));
  const age = workAge(row.work.at);
  if (age) parts.push(dim(age));
  if (row.work.stale) parts.push(yellow("stale"));
  return parts.join(" ");
}

function agentCell(row: FleetRow): string {
  if (!row.session.alive) return dim("—");
  if (row.session.busy === null) return dim("live");
  return row.session.busy ? yellow("busy") : "idle";
}

function prCell(row: FleetRow): string {
  const pr = row.pr;
  if (!pr) return dim("—");
  const parts = [`#${pr.number}`];
  if (pr.state === "MERGED") parts.push(green("merged"));
  else if (pr.state === "CLOSED") parts.push(dim("closed"));
  else if (pr.isDraft) parts.push(dim("draft"));
  else parts.push("open");
  return parts.join(" ");
}

function mergeCell(row: FleetRow): string {
  const pr = row.pr;
  if (!pr || pr.state !== "OPEN") return dim("—");
  const state = mergeField(pr.mergeStateStatus, pr.state);
  const mergeable = mergeField(pr.mergeable, pr.state);
  if (!state && !mergeable) return dim("—");
  const paint = (v: string): string =>
    v === "clean" || v === "mergeable"
      ? green(v)
      : v === "dirty" || v === "conflicting" || v === "blocked"
        ? red(v)
        : v === "computing"
          ? dim(v)
          : yellow(v);
  return [state, mergeable]
    .filter((v): v is string => v !== null)
    .map(paint)
    .join(" ");
}

function ciCell(row: FleetRow): string {
  const pr = row.pr;
  if (!pr || pr.state !== "OPEN") return dim("—");
  switch (pr.checks) {
    case "pass":
      return green("pass");
    case "fail":
      return red("fail");
    case "pending":
      return yellow("pending");
    default:
      return dim("—");
  }
}

export const run = Effect.fn("wt fleet")(function* (argv: string[]) {
  if (hasHelpFlag(argv)) {
    console.log(USAGE);
    return 0;
  }
  const unknown = firstUnknownFlag(argv, KNOWN_FLAGS);
  if (unknown) {
    console.error(red(`unknown flag: ${unknown}`));
    return 2;
  }
  const unexpected = argv.find((arg) => !arg.startsWith("-"));
  if (unexpected) {
    console.error(red(`unexpected argument: ${unexpected}`));
    return 2;
  }
  const json = argv.includes("--json");

  const wts = (yield* listWorktrees()).filter((w) => !w.isMain);
  const wtState = yield* io.sync("read wt state", readWtState);
  const slugStates = wtState.slugs;
  const removed = yield* io.sync("read recently removed worktrees", () =>
    recentlyRemovedWorktrees(new Set(wts.map((w) => w.slug))),
  );
  const branches = wts.filter((w) => w.branch).map((w) => w.branch);

  // Independent realities in parallel: the batched GitHub round trip,
  // tmux session list, and one HEAD resolve per worktree (for status
  // staleness, same signal `wt status --all` uses).
  const [{ prs, note }, sessions, heads, landedFlags] = yield* Effect.all(
    [
      fetchFleetPrs(branches),
      listSessions(),
      Effect.all(
        wts.map((w) => revParse("HEAD", w.path)),
        { concurrency: 8 },
      ),
      Effect.all(
        wts.map((w) =>
          w.branch
            ? branchIsMerged({ slug: w.slug, branch: w.branch, path: w.path }).pipe(
                Effect.flatMap((merged) =>
                  merged
                    ? Effect.succeed(true)
                    : branchIsGone(w.branch, w.path).pipe(
                        // Same safe direction as branchIsMerged: a broken
                        // probe must never assert a branch is gone.
                        Effect.orElseSucceed(() => false),
                      ),
                ),
              )
            : Effect.succeed(false),
        ),
        { concurrency: 8 },
      ),
    ],
    { concurrency: "unbounded" },
  );
  const registry = readRegistry();
  const liveClaudeSlugs = new Set(
    sessions.claude.filter((e) => e.name === null).map((e) => e.slug),
  );
  const liveHarnessSlugs = new Set([
    ...liveClaudeSlugs,
    ...sessions.codex,
    ...sessions.opencode,
  ]);

  // Staleness for edges reuses the HEADs already resolved above (one
  // per live worktree). An endpoint that is not a live worktree maps
  // to null, which `edgeIsStaleBySha` reads as "unknown, not stale by
  // this side" — the same treatment `wt edge` gives it.
  const headBySlug = new Map<string, string | null>(
    wts.map((w, i) => [w.slug, heads[i] ?? null]),
  );
  const edges = wtState.edges.map((e) => ({
    ...e,
    stale: edgeIsStaleBySha(e, (slug) => headBySlug.get(slug) ?? null),
  }));

  const rows: FleetRow[] = wts.map((w, i) => {
    const record = slugStates[w.slug]?.work;
    const headSha = heads[i] ?? null;
    return {
      wt: w,
      base: slugStates[w.slug]?.baseBranch ?? config.branch.base,
      edges: edges.filter((e) => e.from === w.slug || e.to === w.slug),
      section:
        config.instance.role === "worker"
          ? null
          : (slugStates[w.slug]?.section ?? null),
      work: record
        ? {
            ...record,
            stale: !!(record.sha && headSha && record.sha !== headSha),
          }
        : null,
      landed:
        (landedFlags[i] ?? false) ||
        pickPrForWorktree(w, prs)?.state === "MERGED",
      session: sessionInfoFor(w, liveHarnessSlugs, liveClaudeSlugs, registry),
      pr: pickPrForWorktree(w, prs),
    };
  });
  // Urgency order, derived at render time (same ranking the TUI sorts
  // by): ready first, then needs-human, todo last.
  // A landed branch still owing a deployed-environment check ranks as
  // what it now is — needs-testing — rather than as the `ready` it
  // still asserts. Same derivation the TUI's `rowWorkRank` makes, and
  // for the same reason: this is the moment the check became runnable,
  // so it is the wrong moment for the row to go quiet.
  const rank = (r: FleetRow): number =>
    owesPostMergeVerification(r.work, r.landed)
      ? workStateRank("needs-testing")
      : workRecordRank(r.work);
  rows.sort(
    (a, b) => rank(a) - rank(b) || a.wt.slug.localeCompare(b.wt.slug),
  );

  if (json) {
    const payload = [
      ...rows.map((r) => ({
        slug: r.wt.slug,
        branch: r.wt.branch,
        // The effective merge target, never null — trunk when nothing
        // is recorded. Reading a stack off this surface needs it: two
        // rows whose `base` names another row's branch ARE a chain.
        base: r.base,
        path: r.wt.path,
        // Positive discriminator, same value and meaning on every JSON
        // surface that appends removed history — see ls.ts.
        kind: "live" as const,
        // The human's manual grouping in the TUI ("Merge after Release",
        // …) — asserted intent the manager should weigh; null = inbox.
        // Inferred stack groupings deliberately don't appear here: they
        // are derivable reality (base records + PRs), not assertion.
        section: r.section,
        // Pairwise ordering assertions touching this slug, in either
        // direction, verbatim from `wt edge --json` plus `stale`.
        // Spread rather than rebuilt field-by-field: this row schema
        // has already dropped a field that way, and an edge that reads
        // differently depending on which command printed it is the
        // same failure one level down. `stale` means an endpoint moved
        // past its anchor — ignore those for ordering.
        edges: r.edges,
        work: r.work
          ? {
              state: r.work.state,
              note: r.work.note ?? null,
              risk: r.work.risk ?? null,
              // The external merge gate. Non-null means DO NOT MERGE
              // whatever `state` says — this row is the manager's
              // primary sense, and reading `ready` off a gated branch
              // here is the exact failure the field was added for.
              blockedOn: r.work.blockedOn ?? null,
              // A check that can only run once this is DEPLOYED. Read
              // it the OPPOSITE way from `blockedOn`: this row should
              // be merged, and on a merged row a non-null value means
              // the worktree is being held back deliberately and the
              // check has not happened yet.
              verifyAfterMerge: r.work.verifyAfterMerge ?? null,
              at: r.work.at,
              // Agent identity that asserted it — the worktree's own
              // slug normally, `manager` when triage did, null for the
              // human. "Already triaged" is otherwise unreadable.
              by: r.work.by ?? null,
              stale: r.work.stale,
            }
          : null,
        session: r.session,
        pr: r.pr
          ? {
              number: r.pr.number,
              url: r.pr.url,
              title: r.pr.title,
              state: r.pr.state,
              draft: r.pr.isDraft,
              merge_state: mergeField(r.pr.mergeStateStatus, r.pr.state),
              mergeable: mergeField(r.pr.mergeable, r.pr.state),
              checks: r.pr.checks,
              // Open review work, in three separate numbers, because
              // collapsing them is what made this field lie. A `ready`
              // status with anything outstanding is a status/reality
              // mismatch of the family this surface exists to audit —
              // agents have been observed replying via `gh pr comment`
              // (a top-level comment, NOT a thread reply) and leaving
              // every thread open while believing findings addressed.
              //
              // `unresolved_threads` is every open thread, matching the
              // count GitHub's own PR page shows and what a hand-rolled
              // `reviewThreads` query returns. `unresolved_human_threads`
              // excludes bot-opened ones: on a repo where all review is
              // done by a bot that number is permanently 0, so reporting
              // ONLY it reads as "nothing to chase" while the bot sits
              // on unaddressed findings.
              unresolved_threads: r.pr.unresolvedThreadsTotal,
              unresolved_human_threads: r.pr.unresolvedThreads,
              // The review bot's own rollup — in `checklist` mode this
              // is the unticked-box count from its summary comment,
              // which is the number the PR page surfaces and which
              // thread resolution does NOT affect. A PR can show zero
              // unresolved threads and still read as having open
              // findings; this is that other half.
              review_bot: r.pr.reviewBot
                ? {
                    state: r.pr.reviewBot.state,
                    unresolved: r.pr.reviewBot.unresolved,
                    // Whether the bot has reviewed THIS head. Without it
                    // `clean` is two different answers wearing one word —
                    // "looked at your latest push and found nothing" and
                    // "looked at an older commit" — and the TUI already
                    // refuses to paint the second one green. Dropping the
                    // flag here handed every JSON reader exactly the green
                    // reading the badge withholds, which matters most to
                    // the caller that acts on it: an agent iterating a
                    // bot's follow-up reviews stops at the first stale
                    // clean, believing its newest commit came back empty.
                    // Checklist mode only; `false` elsewhere, matching
                    // what every existing reader already infers from the
                    // absent field.
                    stale: r.pr.reviewBot.stale ?? false,
                  }
                : null,
            }
          : null,
        // Distinguishes "no PR" (pr null, pr_note null) from "GitHub
        // unavailable" (pr null, pr_note says why).
        pr_note: r.pr ? null : note,
      })),
      ...removed.map(removedJsonEntry),
    ];
    console.log(JSON.stringify(payload, null, 2));
    return 0;
  }

  if (rows.length === 0 && removed.length === 0) {
    console.log(dim("No worktrees."));
    return 0;
  }
  if (rows.length > 0) {
    const table = renderTable(rows as unknown[], [
      { header: "slug", getter: (r) => cyan((r as FleetRow).wt.slug) },
      {
        header: "section",
        getter: (r) => dim((r as FleetRow).section ?? "—"),
      },
      { header: "work", getter: (r) => workCell(r as FleetRow) },
      { header: "agent", getter: (r) => agentCell(r as FleetRow) },
      { header: "pr", getter: (r) => prCell(r as FleetRow) },
      { header: "merge", getter: (r) => mergeCell(r as FleetRow) },
      { header: "ci", getter: (r) => ciCell(r as FleetRow) },
    ]);
    console.log(table);
  }
  // The ordering constraints, on the surface that plans merges. An
  // edge whose endpoint is not a live row says nothing about this
  // fleet; a stale one is ignored by ordering everywhere else, so it
  // is counted rather than listed.
  const liveSlugs = new Set(rows.map((r) => r.wt.slug));
  const fleetEdges = edges.filter(
    (e) => liveSlugs.has(e.from) && liveSlugs.has(e.to),
  );
  if (fleetEdges.length > 0) {
    const fresh = fleetEdges.filter((e) => !e.stale);
    console.log("");
    console.log(dim("merge edges:"));
    for (const e of fresh) {
      const arrow = e.kind === "conflicts" ? "×" : "▶";
      const strength =
        e.strength === "blocks" ? red("blocks") : dim("prefer");
      const why = e.why ? dim(` · ${e.why}`) : "";
      console.log(
        `  ${cyan(e.from)} ${dim(`─${e.kind}─${arrow}`)} ${cyan(e.to)}   ${strength}${why}`,
      );
    }
    const staleCount = fleetEdges.length - fresh.length;
    if (staleCount > 0) {
      console.log(
        dim(
          `  ${staleCount} stale (endpoint moved) — \`wt edge\` lists them`,
        ),
      );
    }
  }
  if (note) console.log(dim(`note: ${note}`));
  if (removed.length > 0) {
    console.log("");
    console.log(dim("recently removed:"));
    for (const e of removed) {
      const entry = removedJsonEntry(e);
      const pr = entry.pr !== null ? ` #${entry.pr}` : "";
      const age = workAge(entry.archived_at);
      console.log(
        dim(
          `  ${entry.slug}  ${entry.kind}${pr}${age ? `, ${age} ago` : ""}`,
        ),
      );
      // Loud, and only here: a row whose checkout is gone while a
      // deployed-environment check was still owed has nowhere else left
      // to be reported.
      if (entry.verification_owed) {
        console.log(
          yellow(
            `    UNVERIFIED — owed: ${verifyStepsHeadline(entry.verify_after_merge!)}`,
          ),
        );
      }
    }
  }
  return 0;
});
