/**
 * The `wt events` webhook daemon: a long-lived loopback HTTP server that
 * turns GitHub webhook deliveries into github-query refreshes.
 *
 * It is a *signal*, not a data source. A delivery never reconstructs
 * `GithubData` from the (differently-shaped) webhook payload; it just
 * tells the daemon something changed on one of our worktree branches, and
 * the daemon re-runs the same batched `fetchGithub` the TUI uses, writes a
 * snapshot, and rewrites the marker. The TUI picks up the marker and reads
 * the warm snapshot. One bounded GraphQL round-trip per burst, debounced.
 *
 * Auth is the webhook HMAC secret (`X-Hub-Signature-256`) and nothing
 * else: a plain repo webhook, no GitHub App, no installation token. Data
 * still flows through the user's existing `gh` auth.
 */
import { createHmac, timingSafeEqual } from "node:crypto";
import { readFileSync } from "node:fs";

import { config, type GithubEventsConfig } from "../config.ts";
import { fetchGithub } from "../github.ts";
import { createLogger } from "../logger.ts";
import { listWorktrees } from "../worktree.ts";

import {
  ensureEventsDir,
  touchMarker,
  writeSnapshot,
  writeState,
  type EventsState,
} from "./store.ts";

const log = createLogger("[events]");

/** Coalesce check_run/check_suite bursts into one fetch per CI step storm. */
const FETCH_DEBOUNCE_MS = 1_500;
/**
 * Reject webhook bodies larger than this before buffering them. GitHub
 * payloads are well under this (typically <1MB, hard-capped ~25MB), so the
 * cap only bites a malformed or hostile request — which matters once the
 * listener binds beyond loopback (a non-`127.0.0.1` `host`).
 */
const MAX_BODY_BYTES = 5 * 1024 * 1024;
/** Re-read the worktree branch set at most this often when deciding relevance. */
const LOCAL_BRANCHES_TTL_MS = 30_000;

/** Webhook event types worth a refresh. Everything else is dropped. */
const RELEVANT_EVENTS = new Set([
  "pull_request",
  "pull_request_review",
  "check_suite",
  "check_run",
  "status",
  "merge_group",
]);

/**
 * Resolve the HMAC secret: inline wins, else the secret file's trimmed
 * contents, else null (daemon refuses to start — unsigned webhooks are not
 * accepted).
 */
export function resolveWebhookSecret(events: GithubEventsConfig): string | null {
  if (events.secret) return events.secret;
  if (events.secretFile) {
    try {
      const s = readFileSync(events.secretFile, "utf8").trim();
      return s.length > 0 ? s : null;
    } catch {
      return null;
    }
  }
  return null;
}

/** Constant-time verify of GitHub's `sha256=<hex>` body signature. */
function verifySignature(body: string, header: string | null, secret: string): boolean {
  if (!header || !header.startsWith("sha256=")) return false;
  const mac = createHmac("sha256", secret).update(body).digest("hex");
  const expected = `sha256=${mac}`;
  // timingSafeEqual throws on length mismatch; guard first. Both sides are
  // attacker-influenced only via `header`, and the length of a valid hex
  // digest is fixed, so an early length check leaks nothing useful.
  if (expected.length !== header.length) return false;
  return timingSafeEqual(Buffer.from(expected), Buffer.from(header));
}

/**
 * Candidate head branches an event concerns, or null when the shape is
 * unknown / intentionally unscoped (merge_group). Null means "don't try to
 * skip — just refetch", so a payload-shape change degrades to more fetches,
 * never to missed updates.
 */
export function extractBranches(event: string, payload: unknown): string[] | null {
  const p = payload as Record<string, any> | null;
  if (!p) return null;
  try {
    switch (event) {
      case "pull_request":
      case "pull_request_review": {
        const ref = p.pull_request?.head?.ref;
        return typeof ref === "string" ? [ref] : null;
      }
      case "check_suite": {
        const ref = p.check_suite?.head_branch;
        return typeof ref === "string" ? [ref] : null;
      }
      case "check_run": {
        const ref = p.check_run?.check_suite?.head_branch;
        return typeof ref === "string" ? [ref] : null;
      }
      // A status is keyed to a commit SHA; its `branches` array is an
      // unreliable scoping signal (often lists only the default branch, or
      // is empty for feature-branch CI), so a non-empty-but-non-matching
      // list would wrongly SKIP a real CI update — the one non-fail-safe
      // direction. Treat like merge_group: never skip, always refetch.
      case "status":
        return null;
      // merge_group head_ref is a synthetic `gh-readonly-queue/...` ref, not
      // a worktree branch — never skippable, always refetch for the queue.
      case "merge_group":
        return null;
      default:
        return null;
    }
  } catch {
    return null;
  }
}

type Daemon = {
  stop: () => void;
};

/** Start the server + fetch loop. Returns a stop handle; does not block. */
export function startDaemon(events: GithubEventsConfig, secret: string): Daemon {
  ensureEventsDir();

  const state: EventsState = {
    pid: process.pid,
    port: events.port,
    startedAt: Date.now(),
    lastEventAt: null,
    lastFetchAt: null,
    eventCount: 0,
    lastError: null,
  };
  writeState(state);

  let localBranches = new Set<string>();
  let localBranchesAt = 0;
  async function currentBranches(): Promise<string[]> {
    const wts = await listWorktrees();
    return wts
      .filter((w) => !w.isMain && w.branch)
      .map((w) => w.branch as string);
  }
  async function getLocalBranches(): Promise<Set<string>> {
    if (localBranches.size > 0 && Date.now() - localBranchesAt < LOCAL_BRANCHES_TTL_MS) {
      return localBranches;
    }
    localBranches = new Set(await currentBranches());
    localBranchesAt = Date.now();
    return localBranches;
  }

  let fetchTimer: ReturnType<typeof setTimeout> | null = null;
  let fetching = false;
  let refetchQueued = false;

  async function runFetch(): Promise<void> {
    if (fetching) {
      // A burst landed mid-fetch; remember to run once more so the final
      // state always wins.
      refetchQueued = true;
      return;
    }
    fetching = true;
    try {
      const branches = await currentBranches();
      localBranches = new Set(branches);
      localBranchesAt = Date.now();
      const { prs, mergeQueue } = await fetchGithub(branches);
      writeSnapshot({
        updatedAt: Date.now(),
        branches,
        prs: Object.fromEntries(prs),
        mergeQueue: Object.fromEntries(mergeQueue),
      });
      touchMarker(Date.now());
      state.lastFetchAt = Date.now();
      state.lastError = null;
      writeState(state);
      log.info("refetched after webhook", { branches: branches.length, prs: prs.size });
    } catch (err) {
      state.lastError = err instanceof Error ? err.message : String(err);
      writeState(state);
      log.error("webhook refetch failed", { err: state.lastError });
    } finally {
      fetching = false;
      if (refetchQueued) {
        refetchQueued = false;
        void runFetch();
      }
    }
  }

  function scheduleFetch(): void {
    if (fetchTimer) clearTimeout(fetchTimer);
    fetchTimer = setTimeout(() => {
      fetchTimer = null;
      void runFetch();
    }, FETCH_DEBOUNCE_MS);
  }

  async function handleDelivery(event: string, body: string): Promise<void> {
    let payload: unknown;
    try {
      payload = JSON.parse(body);
    } catch {
      log.warn("webhook body not JSON", { event });
      return;
    }
    const branches = extractBranches(event, payload);
    if (branches) {
      try {
        const local = await getLocalBranches();
        if (!branches.some((b) => local.has(b))) {
          log.debug("ignored event for non-local branch", { event, branches });
          return;
        }
      } catch (err) {
        // Couldn't resolve the local branch set (transient git trouble,
        // e.g. an index lock mid-rebase). Don't drop the delivery on a
        // fire-and-forget path — fall through to a refetch. The module's
        // invariant: more fetches, never missed updates.
        log.warn("local-branch check failed; refetching anyway", {
          err: err instanceof Error ? err.message : String(err),
        });
      }
    }
    state.eventCount++;
    state.lastEventAt = Date.now();
    writeState(state);
    scheduleFetch();
  }

  const server = Bun.serve({
    port: events.port,
    hostname: events.host,
    maxRequestBodySize: MAX_BODY_BYTES,
    async fetch(req): Promise<Response> {
      const url = new URL(req.url);
      if (req.method === "GET" && url.pathname === "/health") {
        return Response.json({ ok: true, port: events.port, eventCount: state.eventCount });
      }
      if (req.method !== "POST" || url.pathname !== "/webhook") {
        return new Response("not found", { status: 404 });
      }
      // Early reject oversized bodies (the `maxRequestBodySize` above is the
      // backstop for chunked / no-Content-Length requests).
      const len = Number(req.headers.get("content-length") ?? 0);
      if (Number.isFinite(len) && len > MAX_BODY_BYTES) {
        return new Response("payload too large", { status: 413 });
      }
      const body = await req.text();
      if (!verifySignature(body, req.headers.get("x-hub-signature-256"), secret)) {
        log.warn("webhook signature rejected", {
          delivery: req.headers.get("x-github-delivery"),
        });
        return new Response("invalid signature", { status: 401 });
      }
      const event = req.headers.get("x-github-event") ?? "";
      // GitHub pings the endpoint once on creation — ack it so the webhook
      // shows green in the UI.
      if (event === "ping") return Response.json({ ok: true });
      if (RELEVANT_EVENTS.has(event)) {
        // Don't make GitHub wait on our fetch — ack immediately, process async.
        void handleDelivery(event, body);
      }
      return Response.json({ ok: true });
    },
  });

  log.info("events daemon listening", { host: events.host, port: events.port });
  // Warm the snapshot immediately so a TUI opened right after start has data.
  void runFetch();

  return {
    stop: () => {
      if (fetchTimer) clearTimeout(fetchTimer);
      server.stop(true);
    },
  };
}

/**
 * Foreground entry point for `wt events serve` (and the launchd agent).
 * Resolves the secret, starts the daemon, and parks the process until a
 * termination signal. Returns the intended process exit code.
 */
export async function runDaemonForeground(): Promise<number> {
  const events = config.github.events;
  if (!events) {
    process.stderr.write(
      "wt events: [github.events] is not configured in config.toml\n",
    );
    return 1;
  }
  const secret = resolveWebhookSecret(events);
  if (!secret) {
    // Distinguish "nothing configured" from "secret_file is set but
    // unreadable/empty" — the latter shouldn't send the user to mint a new
    // secret when they already have one.
    const hint = events.secretFile
      ? `couldn't read a secret from ${events.secretFile} — check it exists and is readable, or run \`wt events secret\``
      : "set [github.events].secret or secret_file (run `wt events secret` to generate one)";
    process.stderr.write(`wt events: no webhook secret. ${hint}.\n`);
    return 1;
  }
  let daemon: ReturnType<typeof startDaemon>;
  try {
    daemon = startDaemon(events, secret);
  } catch (err) {
    // The most likely failure is the loopback port already in use (a stale
    // or double-started daemon). Report it cleanly instead of letting the
    // raw Bun.serve throw print a stack trace into the launchd error log.
    const msg = err instanceof Error ? err.message : String(err);
    process.stderr.write(
      `wt events: failed to start on port ${events.port}: ${msg}\n` +
        "(is another daemon already running? check `wt events status`)\n",
    );
    return 1;
  }
  await new Promise<void>((resolve) => {
    const shutdown = () => resolve();
    process.on("SIGTERM", shutdown);
    process.on("SIGINT", shutdown);
  });
  daemon.stop();
  return 0;
}
