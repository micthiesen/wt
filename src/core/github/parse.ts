import { readdirSync } from "node:fs";
import { join } from "node:path";

import { config } from "../config.ts";
import { createLogger } from "../logger.ts";
import type { PrChecks, PrComment, PrReview, PullRequest, ReviewBotStatus } from "../types.ts";
import type {
  GqlCommentAuthor,
  GqlPrNode,
  GqlReviewDecision,
  GqlReviewThread,
  RawCheck,
} from "./types.ts";

const log = createLogger("[gh-parse]");

const CHECK_FAIL_CONCLUSIONS = new Set([
  "FAILURE",
  "CANCELLED",
  "TIMED_OUT",
  "ACTION_REQUIRED",
  "STARTUP_FAILURE",
]);

/** Compile `[github] ignored_checks` (case-insensitive globs, `*` only) into a predicate. */
function compileIgnore(patterns: readonly string[]): (name: string | null | undefined) => boolean {
  if (patterns.length === 0) return () => false;
  const regexes = patterns.map((p) => {
    const escaped = p.replace(/[.+?^${}()|[\]\\]/g, "\\$&").replace(/\*/g, ".*");
    return new RegExp(`^${escaped}$`, "i");
  });
  return (name) => {
    if (!name) return false;
    return regexes.some((r) => r.test(name));
  };
}

// The review bot's own check contexts are excluded from the CI rollup
// unconditionally: the bot has its own badge and an advisory reviewer
// must never flip the checks badge — no need to duplicate them in
// `[github] ignored_checks`.
const isIgnoredCheck = compileIgnore([
  ...config.github.ignoredChecks,
  ...config.reviewBot.checkContexts,
]);

function checkName(c: RawCheck): string | null | undefined {
  return c.__typename === "CheckRun" ? c.name : c.context;
}

function checkStartedAt(c: RawCheck): string | null | undefined {
  return c.__typename === "CheckRun" ? c.startedAt : c.createdAt;
}

/**
 * The Actions run behind a check entry: which workflow file, and which of
 * that file's runs. Null for anything that is not an Actions check run —
 * a StatusContext, a GitHub App's check, or an entry restored from a
 * cache written before these fields were fetched. Every such entry is
 * left to the name dedupe, so an unknown provenance never discards
 * anything.
 */
function workflowRunOf(c: RawCheck): { run: number; workflow: number } | null {
  if (c.__typename !== "CheckRun") return null;
  const run = c.checkSuite?.workflowRun?.databaseId;
  const workflow = c.checkSuite?.workflowRun?.workflow?.databaseId;
  if (typeof run !== "number" || typeof workflow !== "number") return null;
  return { run, workflow };
}

/**
 * Predicate: this entry belongs to a run that a later run of the same
 * workflow has replaced. Ids are per-repo monotonic, so "later" needs no
 * timestamp — which matters, because the orphaned entries this exists for
 * are precisely the ones with nothing to compare against.
 */
function supersededRuns(raw: readonly RawCheck[]): (c: RawCheck) => boolean {
  const newestRun = new Map<number, number>();
  for (const c of raw) {
    const w = workflowRunOf(c);
    if (!w) continue;
    const prev = newestRun.get(w.workflow);
    if (prev === undefined || w.run > prev) newestRun.set(w.workflow, w.run);
  }
  return (c) => {
    const w = workflowRunOf(c);
    if (!w) return false;
    return w.run < (newestRun.get(w.workflow) ?? w.run);
  };
}

/**
 * `statusCheckRollup` is HISTORY, not state: GitHub keeps every check
 * run recorded against a head sha forever, so a run that failed and was
 * then re-run green leaves BOTH entries in the rollup for the same
 * context name. Reading it undeduped means one superseded failure pins
 * the badge red for the life of the branch, on a PR whose
 * `mergeStateStatus` is CLEAN and whose `gh pr checks` is all green.
 *
 * That is not a rare shape. A retried flake, a cancelled run, a draft
 * that failed a job gated on non-draft and was then marked ready — all
 * produce it, and the PR can never come back green on its own. It also
 * pushes an agent toward the worst available remedy: an empty commit to
 * move the sha and silence the badge, which spends a CI run to clear a
 * false alarm.
 *
 * So collapse to the newest run per context. **Ordering is only applied
 * where it is KNOWN**: entries whose timestamp is missing (an older
 * persisted cache, or a context type that does not carry one) are all
 * kept, which preserves the previous any-failure-counts behaviour for
 * them. That direction is deliberate — a false red costs a look, while
 * a false green is a broken branch reported as fine.
 *
 * Collapsing BY NAME is not enough on its own, because a superseded run
 * does not always leave a same-named successor to lose to. When a run is
 * cancelled before its matrix expands, GitHub records the job under its
 * literal unexpanded name — `Integration tests (Vitest ${{ matrix.shard
 * }}/3)` — while the run that replaces it reports `1/3`, `2/3`, `3/3`.
 * Those names can never meet, so a CANCELLED placeholder pinned the badge
 * red for the life of the branch on a PR whose `mergeStateStatus` was
 * CLEAN and every real check green. A job renamed between two runs of the
 * same sha does the same thing. So supersession is resolved at the level
 * it actually happens — the WORKFLOW RUN — before names are compared at
 * all: within one head sha, a later run of the same workflow file
 * discards the whole of an earlier one, orphaned names included.
 */
function latestPerContext(raw: readonly RawCheck[]): RawCheck[] {
  const superseded = supersededRuns(raw);
  const newest = new Map<string, { at: number; check: RawCheck }>();
  const undated: RawCheck[] = [];
  for (const c of raw) {
    if (superseded(c)) continue;
    const name = checkName(c);
    const at = Date.parse(checkStartedAt(c) ?? "");
    if (!name || !Number.isFinite(at)) {
      undated.push(c);
      continue;
    }
    const prev = newest.get(name);
    if (!prev || at >= prev.at) newest.set(name, { at, check: c });
  }
  // Undated first so the caller's order is stable-ish for name lists;
  // nothing downstream depends on the exact order.
  return [...undated, ...[...newest.values()].map((v) => v.check)];
}

export function rollupChecks(raw: RawCheck[] | null | undefined): PrChecks {
  if (!raw || raw.length === 0) return "none";
  let pending = false;
  let fail = false;
  let counted = 0;
  for (const c of latestPerContext(raw)) {
    if (isIgnoredCheck(checkName(c))) continue;
    counted++;
    if (c.__typename === "CheckRun") {
      if (c.status && c.status !== "COMPLETED") pending = true;
      else if (c.conclusion && CHECK_FAIL_CONCLUSIONS.has(c.conclusion)) fail = true;
    } else {
      const s = c.state;
      if (s === "PENDING" || s === "EXPECTED") pending = true;
      else if (s === "FAILURE" || s === "ERROR") fail = true;
    }
  }
  if (counted === 0) return "none";
  if (fail) return "fail";
  if (pending) return "pending";
  return "pass";
}

/**
 * Names of the checks currently failing, using the same fail set +
 * ignore filter as `rollupChecks`. Order preserved from the rollup;
 * powers the details-pane "which checks failed" line and the
 * `--log-failed` tail keybind. Empty unless something actually failed.
 */
function failingCheckNames(raw: RawCheck[] | null | undefined): string[] {
  if (!raw) return [];
  const out: string[] = [];
  // Same dedupe as `rollupChecks`, and it must be: a name listed here
  // that the badge does not count reads as wt contradicting itself.
  for (const c of latestPerContext(raw)) {
    const name = checkName(c);
    if (isIgnoredCheck(name)) continue;
    const failed =
      c.__typename === "CheckRun"
        ? !!(c.conclusion && CHECK_FAIL_CONCLUSIONS.has(c.conclusion))
        : c.state === "FAILURE" || c.state === "ERROR";
    if (failed && name) out.push(name);
  }
  return out;
}

/**
 * Floor an OPEN PR's check rollup at `pending` — but only when the repo
 * actually runs CI. GitHub reports an empty rollup in the window between
 * opening a PR and the first check run registering (and momentarily during
 * some background refreshes), which `rollupChecks` maps to `none`. On a
 * CI-wired repo, `none` on an open PR therefore means "checks haven't
 * reported yet" — surface it as `pending` so the badge holds its slot
 * instead of vanishing and shoving the rest of the row's glyph cluster
 * around on each refresh. On a repo with NO CI at all (dotfiles-style),
 * that flooring would pin every open PR at "checks pending" forever, so
 * it's gated on Actions workflow files existing in the main clone.
 * (External-CI repos without workflow files only lose the slot-holding
 * nicety for the seconds before their first status reports.) Closed/merged
 * PRs keep `none` (their check state is genuinely terminal).
 */
const REPO_HAS_CI: boolean = (() => {
  try {
    return readdirSync(join(config.paths.mainClone, ".github", "workflows")).some(
      (f) => f.endsWith(".yml") || f.endsWith(".yaml"),
    );
  } catch {
    // ENOENT = genuinely no workflows dir. Anything else (perms, a
    // not-yet-mounted clone) also degrades to "no CI" for the process
    // lifetime — log it so a mysteriously-vanishing pending badge is
    // diagnosable.
    log.debug("workflows dir unreadable; treating repo as no-CI");
    return false;
  }
})();

export function openPrChecks(
  state: PullRequest["state"],
  checks: PrChecks,
  repoHasCi: boolean = REPO_HAS_CI,
): PrChecks {
  return repoHasCi && state === "OPEN" && checks === "none" ? "pending" : checks;
}

// Review-bot identity, from `[review_bot]` (CodeRabbit preset when the
// section is absent). The strings are user-facing values owned by the
// bot, not us — when they drift, the badge silently disappears
// (state: "none") rather than breaking the whole pane.
const BOT = config.reviewBot;

/**
 * True when `login` is the configured bot. GraphQL reports app logins
 * without the `[bot]` suffix REST adds (`github-actions` vs
 * `github-actions[bot]`); strip it on both sides so either spelling in
 * config matches. Fast-path the exact match — this runs per thread and
 * per comment on every fetch.
 */
function isBotLogin(login: string | null | undefined, expected: string = BOT.login): boolean {
  if (!login) return false;
  if (login === expected) return true;
  return login.replace(/\[bot\]$/, "") === expected;
}

/**
 * Which check names belong to `bot`, compiled once per bot object. The
 * default `BOT` is a module singleton, so the common path builds its
 * regexes exactly once; a test's injected bot gets its own entry and
 * never reaches the machine's `[review_bot]`.
 */
const contextMatchers = new WeakMap<ChecklistBot, (name: string | null | undefined) => boolean>();
function isBotContextOf(bot: ChecklistBot): (name: string | null | undefined) => boolean {
  let m = contextMatchers.get(bot);
  if (!m) {
    m = compileIgnore(bot.checkContexts);
    contextMatchers.set(bot, m);
  }
  return m;
}

/**
 * Aggregate state of the bot's own check contexts on the current head:
 * `pending` when any is still running, `done` when at least one exists
 * and all completed, null when none are attached to this commit (the
 * bot never ran here — or ran on an older commit).
 */
function botContextState(
  contexts: RawCheck[] | null | undefined,
  bot: ChecklistBot = BOT,
): "pending" | "done" | null {
  const isBotContext = isBotContextOf(bot);
  let state: "pending" | "done" | null = null;
  for (const c of contexts ?? []) {
    if (!isBotContext(checkName(c))) continue;
    const pending =
      c.__typename === "CheckRun"
        ? !!(c.status && c.status !== "COMPLETED")
        : c.state === "PENDING" || c.state === "EXPECTED";
    if (pending) return "pending";
    state = "done";
  }
  return state;
}

const UNTICKED_BOX_RE = /^\s*- \[ \]/;
const FENCE_RE = /^\s*(```|~~~)/;

/**
 * Unticked `- [ ]` items in a summary-comment task list. Fenced code
 * blocks are skipped — a suggestion block quoting checkbox syntax must
 * not inflate the count. Indented (nested) checkboxes outside fences DO
 * count: GitHub renders them as real, tickable boxes. Exported for tests.
 */
export function countUntickedBoxes(body: string): number {
  let n = 0;
  let inFence = false;
  for (const line of body.split("\n")) {
    if (FENCE_RE.test(line)) {
      inFence = !inFence;
      continue;
    }
    if (!inFence && UNTICKED_BOX_RE.test(line)) n++;
  }
  return n;
}

/**
 * `checklist` mode: the bot posts a summary comment (identified by
 * `summary_marker`) whose `- [ ]` task list is the unresolved signal —
 * ticking a box in the GitHub UI is how items get accepted/dismissed.
 * Thread resolution is NOT the signal here, and the author login can't
 * be (it's typically `github-actions`, shared with every workflow).
 *
 * Staleness: this kind of bot deliberately skips re-running on push, so
 * a summary older than the head commit's `committedDate` describes an
 * older diff — flagged, not hidden. (Check contexts can't carry this
 * signal: comment-triggered re-runs never attach check runs to the PR
 * head at all, which is also why `pending` derives from a
 * `pending_marker` ack comment newer than the latest summary.)
 *
 * Comment-edit awareness: the latest summary is picked by `createdAt`
 * (edits must not promote an older summary over a newer one), but the
 * ack-vs-summary pending comparison uses the last-touched time so a bot
 * that updates its summary in place — or a human ticking boxes — counts
 * as fresher than an earlier ack.
 */
/**
 * Number of leading lines a `summary_marker` / `pending_marker` may
 * appear on. Not a prefix test, because the standard way to make a
 * comment machine-identifiable is an HTML comment on its own first
 * line (`<!-- codex-review-summary -->`) followed by the visible
 * heading — a shape a strict prefix match rejects, silently, leaving a
 * blank badge with nothing to debug. Two repos running variants of the
 * same reviewer workflow differed on exactly this.
 *
 * Bounded to the opening lines rather than the whole body so a human
 * (or the bot) quoting the heading deep in a long comment can't
 * promote that comment to "the summary".
 */
const MARKER_SCAN_LINES = 3;

export function hasMarker(body: string, marker: string): boolean {
  if (body.startsWith(marker)) return true;
  return body
    .split("\n", MARKER_SCAN_LINES)
    .some((line) => line.trimStart().startsWith(marker));
}

type BotComment = { body: string; createdAt: string; updatedAt?: string | null };

/**
 * The longest configured summary marker this body carries, or null.
 *
 * Longest rather than first, because markers are prefixes and one can
 * contain another: `### Review` would claim `### Review follow-up`'s
 * comments and file both under one key, silently keeping only whichever
 * was newer. Nothing in the shipped config collides today; the cost of
 * removing the trap is one comparison.
 */
function summaryMarkerOf(body: string, markers: readonly string[]): string | null {
  let best: string | null = null;
  for (const m of markers) {
    if (!hasMarker(body, m)) continue;
    if (best === null || m.length > best.length) best = m;
  }
  return best;
}

/**
 * The slice of `[review_bot]` this rollup reads. Injectable so the tests
 * state their own markers instead of inheriting whichever bot the
 * machine running them is configured for — the suite would otherwise be
 * green on a checklist config and vacuous on a threads one, which is the
 * shape of an instrument answering about a world it is not in.
 *
 * It has to carry the WHOLE identity, `checkContexts` included. Leaving
 * that one field to read the ambient config left the injection half
 * done, in the way that reads as working: the comment side was hermetic
 * while the check-context side still asked the machine, so four tests
 * passed on a Codex-configured laptop and failed in CI, whose synthetic
 * config has no `[review_bot]` at all.
 */
export type ChecklistBot = {
  login: string;
  checkContexts: readonly string[];
  summaryMarkers: readonly string[];
  pendingMarker: string | null;
};

/**
 * Does a live checklist say, in its own words, that it covers this
 * commit?
 *
 * A per-commit reviewer NAMES the sha it looked at, and that is the bot
 * asserting its own coverage rather than wt inferring it. Codex heads
 * each delta section with the short sha and repeats the full one in a
 * machine-readable trailer:
 *
 *     #### `28daaa2`
 *     ...
 *     <!-- codex-review-state:v1 {"version":1,...,"head":"28daaa28…"} -->
 *
 * Matched as a bare 7-char prefix rather than against either of those
 * shapes, because both are one bot's formatting and the sha is the fact.
 * A positive signal only: not finding it means the older proxies decide,
 * so a bot that never mentions shas behaves exactly as before.
 */
function coversHead(bodies: readonly string[], headOid: string | null): boolean {
  if (!headOid || headOid.length < 7) return false;
  const short = headOid.slice(0, 7);
  return bodies.some((b) => b.includes(short));
}

export function rollupChecklist(
  contexts: RawCheck[] | null | undefined,
  comments: GqlPrNode["comments"],
  headCommittedDate: string | null,
  headOid: string | null,
  bot: ChecklistBot = BOT,
): ReviewBotStatus {
  // One live checklist PER MARKER, latest-wins within each. A bot can
  // keep several at once and they are independent: a fresh full pass
  // supersedes the previous FULL PASS and nothing else, while the delta
  // log accumulates alongside it. Keeping a single latest-of-all comment
  // is what hid an open finding behind a green badge — the newest
  // comment happened to be the one with no boxes left.
  const summaries = new Map<string, BotComment>();
  let ack: BotComment | null = null;
  for (const c of comments?.nodes ?? []) {
    if (!c?.body || !isBotLogin(c.author?.login, bot.login)) continue;
    const marker = summaryMarkerOf(c.body, bot.summaryMarkers);
    if (marker !== null) {
      const prev = summaries.get(marker);
      if (!prev || c.createdAt > prev.createdAt) summaries.set(marker, c);
    } else if (bot.pendingMarker && hasMarker(c.body, bot.pendingMarker)) {
      if (!ack || c.createdAt > ack.createdAt) ack = c;
    }
  }
  const live = [...summaries.values()];
  const touchedAt = (c: BotComment): string =>
    c.updatedAt && c.updatedAt > c.createdAt ? c.updatedAt : c.createdAt;

  const ctx = botContextState(contexts, bot);
  // "Stale" is a claim that the bot never saw THIS commit, and there are
  // three ways to answer it, in descending order of directness.
  //
  // The bot's own words come first: a per-commit reviewer stamps the sha
  // it reviewed, which is an assertion rather than an inference, and it
  // is the only one of the three that is true the moment the review is
  // posted. The check contexts are next — the rollup is read off the
  // head commit, so a bot context here means its pipeline ran here — but
  // they register minutes after the comment does, and that gap is a
  // window where a completed review reads as stale.
  //
  // The timestamp proxy is last and has to BE last: the delta log is one
  // comment appended to per commit, so its `createdAt` is the first
  // delta's and reads stale forever after.
  const newest = live.reduce<string | null>(
    (acc, c) => (acc === null || c.createdAt > acc ? c.createdAt : acc),
    null,
  );
  const stale =
    newest !== null &&
    !coversHead(live.map((c) => c.body), headOid) &&
    ctx === null &&
    headCommittedDate !== null &&
    newest < headCommittedDate;

  let unresolved = 0;
  for (const c of live) unresolved += countUntickedBoxes(c.body);
  if (unresolved > 0) return { state: "unresolved", unresolved, stale };

  const newestTouched = live.reduce<string | null>(
    (acc, c) => (acc === null || touchedAt(c) > acc ? touchedAt(c) : acc),
    null,
  );
  const rerunning =
    ack !== null && (newestTouched === null || ack.createdAt > newestTouched);
  if (ctx === "pending" || rerunning) {
    return { state: "pending", unresolved: 0 };
  }
  if (live.length > 0) return { state: "clean", unresolved: 0, stale };
  return { state: "none", unresolved: 0 };
}

function rollupReviewBot(
  state: PullRequest["state"],
  contexts: RawCheck[] | null | undefined,
  threads: GqlReviewThread[] | null | undefined,
  comments: GqlPrNode["comments"],
  headCommittedDate: string | null,
  headOid: string | null,
): ReviewBotStatus {
  if (state !== "OPEN") return { state: "none", unresolved: 0 };
  if (BOT.unresolvedVia === "checklist") {
    return rollupChecklist(contexts, comments, headCommittedDate, headOid);
  }

  // `threads` mode (CodeRabbit): unresolved bot-authored review threads.
  let unresolved = 0;
  for (const t of threads ?? []) {
    if (t.isResolved) continue;
    if (isBotLogin(t.comments.nodes[0]?.author?.login)) unresolved++;
  }
  // Unresolved feedback takes precedence over a fresh re-run — pushes
  // re-trigger the bot routinely and the old threads are still what
  // needs addressing.
  if (unresolved > 0) return { state: "unresolved", unresolved };

  // The bot's check context distinguishes "still running" from "done
  // with no findings" from "never ran / not configured".
  const ctxState = botContextState(contexts);
  if (ctxState === "pending") return { state: "pending", unresolved: 0 };
  if (ctxState === "done") return { state: "clean", unresolved: 0 };
  return { state: "none", unresolved: 0 };
}

function rollupReview(
  state: PullRequest["state"],
  decision: GqlReviewDecision,
  outstandingRequests: number,
  hasStaleChangesRequest: boolean,
): PrReview {
  // Terminal PRs don't carry useful review state; suppress to keep the
  // line uncluttered. The PR badge already conveys merged/closed.
  if (state !== "OPEN") return "none";
  if (decision === "APPROVED") return "approved";
  if (decision === "CHANGES_REQUESTED") {
    // GitHub keeps `reviewDecision` pinned at CHANGES_REQUESTED until
    // the same reviewer submits a new review or the old one is
    // dismissed — re-requesting review doesn't clear it. When the
    // reviewer who CR'd is back in `reviewRequests`, the practical
    // state is "fixed it, asked again, waiting" so surface that as
    // pending instead of leaving the stale "changes requested" badge
    // up.
    if (hasStaleChangesRequest) return "pending";
    return "changes_requested";
  }
  // Distinguish "nobody asked yet" from "asked, waiting" — the action
  // for the first is to hit `v`, for the second it's to wait.
  if (outstandingRequests === 0) return "unrequested";
  return "pending";
}

/**
 * True when at least one reviewer whose latest review was
 * CHANGES_REQUESTED has been re-requested (their login is back in the
 * currently-requested set). Walks `reviews` newest-to-oldest and only
 * keeps each author's most recent terminal verdict so a CR followed by
 * a fresh APPROVED doesn't keep flagging the author.
 */
function hasStaleChangesRequest(
  reviews: GqlPrNode["reviews"],
  currentlyRequested: readonly string[],
): boolean {
  if (currentlyRequested.length === 0) return false;
  const requested = new Set(currentlyRequested);
  const seen = new Set<string>();
  const nodes = reviews?.nodes ?? [];
  for (let i = nodes.length - 1; i >= 0; i--) {
    const r = nodes[i];
    const author = r?.author?.login;
    if (!author || seen.has(author)) continue;
    // Only verdict-bearing states reset the author's running state.
    // COMMENTED and PENDING are non-terminal and should be skipped so
    // an idle "commented" after a CR doesn't mask the CR.
    if (r.state !== "APPROVED" && r.state !== "CHANGES_REQUESTED" && r.state !== "DISMISSED") {
      continue;
    }
    seen.add(author);
    if (r.state === "CHANGES_REQUESTED" && requested.has(author)) return true;
  }
  return false;
}

function extractRequestedReviewers(
  rr: GqlPrNode["reviewRequests"],
): string[] {
  const out: string[] = [];
  for (const node of rr?.nodes ?? []) {
    const r = node.requestedReviewer;
    if (!r) continue;
    if (r.__typename === "User") out.push(r.login);
    else if (r.__typename === "Team") out.push(r.combinedSlug);
  }
  return out;
}

function extractSuggestedReviewers(
  sr: GqlPrNode["suggestedReviewers"],
): import("../types.ts").SuggestedReviewer[] {
  const out: import("../types.ts").SuggestedReviewer[] = [];
  for (const s of sr ?? []) {
    if (!s.reviewer?.login) continue;
    out.push({
      login: s.reviewer.login,
      isAuthor: s.isAuthor,
      isCommenter: s.isCommenter,
    });
  }
  return out;
}

/** Most comments the details pane keeps; older ones drop off the tail. */
const COMMENT_LIMIT = 10;

/**
 * A human authored this — has a login, isn't a GraphQL `Bot`, and isn't
 * the configured review bot (which posts as a user-shaped app on some
 * installs, so the `Bot` check alone can miss it). The bot has its own
 * badge + finding count and would otherwise drown the human
 * conversation.
 */
function isHumanAuthor(
  a: GqlCommentAuthor,
): a is { login: string; __typename?: string } {
  if (!a?.login) return false;
  if (a.__typename === "Bot") return false;
  if (isBotLogin(a.login)) return false;
  return true;
}

/**
 * The PR's human conversation: issue comments + non-empty review bodies,
 * merged and sorted newest-first, bots excluded, capped at
 * `COMMENT_LIMIT`. Inline review-thread comments are deliberately left
 * out — they're summarized by `countUnresolvedHumanThreads` instead of
 * inlined, which would flood the pane. ISO timestamps sort
 * lexicographically, so a string compare orders them chronologically.
 */
function extractComments(pr: GqlPrNode): PrComment[] {
  const out: PrComment[] = [];
  for (const c of pr.comments?.nodes ?? []) {
    const body = c?.body?.trim();
    if (!body || !isHumanAuthor(c.author)) continue;
    out.push({ author: c.author.login, body, createdAt: c.createdAt });
  }
  for (const r of pr.reviews?.nodes ?? []) {
    const body = r?.body?.trim();
    if (!body || !isHumanAuthor(r.author)) continue;
    out.push({ author: r.author.login, body, createdAt: r.createdAt });
  }
  out.sort((a, b) =>
    a.createdAt < b.createdAt ? 1 : a.createdAt > b.createdAt ? -1 : 0,
  );
  return out.slice(0, COMMENT_LIMIT);
}

/**
 * Count of unresolved review threads opened by a human. Reuses the
 * thread nodes already fetched for the review-bot rollup; a thread's
 * opener is its first comment's author.
 */
function countUnresolvedHumanThreads(
  threads: GqlReviewThread[] | null | undefined,
): number {
  let n = 0;
  for (const t of threads ?? []) {
    if (t.isResolved) continue;
    if (!isHumanAuthor(t.comments.nodes[0]?.author ?? null)) continue;
    n++;
  }
  return n;
}

/** Unresolved threads regardless of author — what the PR page shows. */
function countUnresolvedThreads(
  threads: GqlReviewThread[] | null | undefined,
): number {
  let n = 0;
  for (const t of threads ?? []) if (!t.isResolved) n++;
  return n;
}

export function nodeToPr(pr: GqlPrNode): PullRequest {
  const headCommit = pr.commits.nodes[0]?.commit;
  const contexts = headCommit?.statusCheckRollup?.contexts?.nodes ?? null;
  const threads = pr.reviewThreads?.nodes ?? null;
  const requestedReviewers = extractRequestedReviewers(pr.reviewRequests);
  return {
    id: pr.id,
    number: pr.number,
    url: pr.url,
    title: pr.title,
    headRefName: pr.headRefName,
    headRefOid: pr.headRefOid ?? undefined,
    baseRefName: pr.baseRefName,
    mergeCommitOid: pr.mergeCommit?.oid ?? null,
    isDraft: pr.isDraft,
    state: pr.state,
    mergeable: pr.mergeable ?? null,
    mergeStateStatus: pr.mergeStateStatus ?? null,
    checks: openPrChecks(pr.state, rollupChecks(contexts)),
    failedChecks: pr.state === "OPEN" ? failingCheckNames(contexts) : [],
    review: rollupReview(
      pr.state,
      pr.reviewDecision,
      pr.reviewRequests?.totalCount ?? 0,
      hasStaleChangesRequest(pr.reviews, requestedReviewers),
    ),
    reviewRequests: pr.reviewRequests?.totalCount ?? 0,
    reviewBot: rollupReviewBot(
      pr.state,
      contexts,
      threads,
      pr.comments,
      headCommit?.committedDate ?? null,
      pr.headRefOid ?? null,
    ),
    requestedReviewers,
    suggestedReviewers: extractSuggestedReviewers(pr.suggestedReviewers),
    autoMerge: pr.autoMergeRequest
      ? {
          enabledAt: pr.autoMergeRequest.enabledAt,
          mergeMethod: pr.autoMergeRequest.mergeMethod,
        }
      : null,
    comments: extractComments(pr),
    unresolvedThreads: countUnresolvedHumanThreads(threads),
    unresolvedThreadsTotal: countUnresolvedThreads(threads),
    mergedAt: pr.mergedAt ?? null,
    closedAt: pr.closedAt ?? null,
  };
}
