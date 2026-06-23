/**
 * `wt events` — manage the optional GitHub webhook daemon.
 *
 * The daemon (`serve`) is a long-lived loopback HTTP server that refreshes
 * the github query on webhook delivery instead of polling. `install`
 * writes a launchd agent; `start`/`stop` load/unload it; `status` reports
 * liveness; `secret` mints the HMAC secret you paste into the repo
 * webhook. See `core/events/` for the daemon + on-disk contract.
 */
import { randomBytes } from "node:crypto";
import { chmodSync, existsSync, mkdirSync, rmSync, writeFileSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";

import { config } from "../../core/config.ts";
import { resolveWebhookSecret, runDaemonForeground } from "../../core/events/daemon.ts";
import {
  EVENTS_DIR,
  ensureEventsDir,
  isProcessAlive,
  readSnapshot,
  readState,
} from "../../core/events/store.ts";
import { run as sh } from "../../core/proc.ts";
import { bold, cyan, dim, green, red, yellow } from "../colors.ts";

const LAUNCHD_LABEL = "com.wt.events";

const USAGE = `usage: wt events <subcommand>

Manage the optional GitHub webhook daemon. Requires a [github.events]
section in config.toml.

subcommands:
  serve       run the daemon in the foreground (what launchd invokes)
  status      show daemon liveness, last event, snapshot age
  install     write the launchd agent (+ generate a secret if needed)
  uninstall   stop and remove the launchd agent
  start       load the launchd agent (launchctl)
  stop        unload the launchd agent
  secret      generate + store an HMAC secret, print webhook setup`;

function plistPath(): string {
  return join(homedir(), "Library", "LaunchAgents", `${LAUNCHD_LABEL}.plist`);
}

/** The wt entrypoint this daemon runs from — same source tree as this CLI. */
function mainEntry(): string {
  return join(import.meta.dir, "..", "..", "main.ts");
}

function ago(ts: number | null | undefined): string {
  if (!ts) return "never";
  const s = Math.floor((Date.now() - ts) / 1000);
  if (s < 60) return `${s}s ago`;
  if (s < 3600) return `${Math.floor(s / 60)}m ago`;
  if (s < 86400) return `${Math.floor(s / 3600)}h ago`;
  return `${Math.floor(s / 86400)}d ago`;
}

function xmlEscape(s: string): string {
  return s
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;");
}

function plistContents(): string {
  const argv = [process.execPath, mainEntry(), "events", "serve"];
  const env: Record<string, string> = {
    // launchd starts with a minimal PATH; bake the install-time PATH (which
    // has gh + git) plus bun's own dir so `fetchGithub` can shell out.
    PATH: `${dirname(process.execPath)}:${process.env.PATH ?? "/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin"}`,
    HOME: homedir(),
  };
  // Carry config overrides so the daemon loads the same config.toml the TUI does.
  if (process.env.WT_CONFIG) env.WT_CONFIG = process.env.WT_CONFIG;
  if (process.env.XDG_CONFIG_HOME) env.XDG_CONFIG_HOME = process.env.XDG_CONFIG_HOME;

  const argLines = argv.map((a) => `    <string>${xmlEscape(a)}</string>`).join("\n");
  const envLines = Object.entries(env)
    .map(([k, v]) => `    <key>${xmlEscape(k)}</key>\n    <string>${xmlEscape(v)}</string>`)
    .join("\n");
  const outLog = join(EVENTS_DIR, "daemon.out.log");
  const errLog = join(EVENTS_DIR, "daemon.err.log");

  return `<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>${LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
${argLines}
  </array>
  <key>EnvironmentVariables</key>
  <dict>
${envLines}
  </dict>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>${xmlEscape(outLog)}</string>
  <key>StandardErrorPath</key>
  <string>${xmlEscape(errLog)}</string>
</dict>
</plist>
`;
}

/** Print the values to paste into the repo's GitHub webhook settings. */
function printWebhookSetup(host: string, port: number, secretLine: string): void {
  console.log(`\n${bold("GitHub webhook settings")} (repo → Settings → Webhooks → Add webhook):`);
  console.log(`  ${dim("Payload URL")}    https://<your-domain>/webhook   ${dim(`(forward → ${host}:${port}/webhook)`)}`);
  console.log(`  ${dim("Content type")}   application/json`);
  console.log(`  ${dim("Secret")}         ${secretLine}`);
  console.log(`  ${dim("SSL")}            enabled`);
  console.log(`  ${dim("Events")}         pull_request, pull_request_review, check_suite,`);
  console.log(`                 check_run, status, merge_group`);
  console.log(dim("\nAfter saving, use the webhook's \"Recent Deliveries\" → Redeliver to test."));
}

type SecretInfo = { secret: string; alreadyConfigured: boolean; statusLine: string };

function ensureSecret(): SecretInfo | null {
  const events = config.github.events;
  if (!events) return null;
  const existing = resolveWebhookSecret(events);
  if (existing) {
    return { secret: existing, alreadyConfigured: true, statusLine: dim("(already configured)") };
  }
  const secret = randomBytes(32).toString("hex");
  if (events.secretFile) {
    mkdirSync(dirname(events.secretFile), { recursive: true });
    // Create restricted from the start (mode applies on create) AND chmod
    // for the overwrite case (mode is ignored when the file already exists),
    // so the secret is never briefly world-readable.
    writeFileSync(events.secretFile, `${secret}\n`, { mode: 0o600 });
    chmodSync(events.secretFile, 0o600);
    return { secret, alreadyConfigured: false, statusLine: `${green("written")} ${dim(`→ ${events.secretFile}`)}` };
  }
  // No secret_file configured — print it for the user to wire in manually.
  console.log(
    yellow(
      "\nNo [github.events].secret_file configured. Add this to config.toml under [github.events]:",
    ),
  );
  console.log(`  secret = "${secret}"`);
  return { secret, alreadyConfigured: false, statusLine: dim("(shown above)") };
}

/** What to show on the "Secret" line of the webhook setup block. */
function secretDisplay(info: SecretInfo): string {
  return info.alreadyConfigured ? dim("(your existing secret)") : info.secret;
}

async function launchctl(action: "load" | "unload"): Promise<number> {
  const plist = plistPath();
  if (!existsSync(plist)) {
    console.error(red(`no launchd agent at ${plist} — run \`wt events install\` first`));
    return 1;
  }
  const r = await sh(["launchctl", action, "-w", plist]);
  if (r.stderr.trim()) process.stderr.write(r.stderr.endsWith("\n") ? r.stderr : `${r.stderr}\n`);
  if (r.exitCode !== 0) {
    console.error(red(`launchctl ${action} failed (exit ${r.exitCode})`));
    return 1;
  }
  console.log(`${green("✓")} ${action === "load" ? "started" : "stopped"} ${LAUNCHD_LABEL}`);
  return 0;
}

function requireEventsConfigured(): boolean {
  if (!config.github.events) {
    console.error(red("[github.events] is not configured in config.toml."));
    console.error(dim("Add a [github.events] section (port, secret_file) to enable the daemon."));
    return false;
  }
  return true;
}

function cmdStatus(): number {
  const events = config.github.events;
  if (!events) {
    console.log(dim("[github.events] not configured — daemon disabled, github query polls on the 60s timer."));
    return 0;
  }
  const state = readState();
  const alive = state ? isProcessAlive(state.pid) : false;
  const snap = readSnapshot();
  console.log(bold("wt events"));
  console.log(`  status        ${alive ? green("running") : red("not running")}`);
  console.log(`  bind          ${events.host}:${events.port}`);
  console.log(`  secret        ${resolveWebhookSecret(events) ? green("set") : red("missing")}`);
  if (state) {
    if (alive) console.log(`  pid           ${state.pid}`);
    console.log(`  started       ${ago(state.startedAt)}`);
    console.log(`  events        ${state.eventCount} ${dim(`(last ${ago(state.lastEventAt)})`)}`);
    console.log(`  last fetch    ${ago(state.lastFetchAt)}`);
    if (state.lastError) console.log(`  last error    ${red(state.lastError)}`);
  }
  console.log(`  snapshot      ${snap ? `${Object.keys(snap.prs).length} PRs, written ${ago(snap.updatedAt)}` : dim("none")}`);
  if (!alive) console.log(dim("\nStart it with `wt events start` (after `wt events install`)."));
  return 0;
}

function cmdInstall(): number {
  if (!requireEventsConfigured()) return 1;
  const events = config.github.events!;
  ensureEventsDir();
  const secret = ensureSecret();
  const plist = plistPath();
  mkdirSync(dirname(plist), { recursive: true });
  writeFileSync(plist, plistContents());
  console.log(`${green("✓")} launchd agent ${dim("→")} ${plist}`);
  if (secret) printWebhookSetup(events.host, events.port, secretDisplay(secret));
  console.log(`\nNext: ${cyan("wt events start")} to load the daemon, then forward your domain to ${events.host}:${events.port}.`);
  return 0;
}

async function cmdUninstall(): Promise<number> {
  const plist = plistPath();
  if (existsSync(plist)) {
    // Best-effort unload before removing so launchd drops the live job.
    await sh(["launchctl", "unload", "-w", plist]);
    rmSync(plist, { force: true });
    console.log(`${green("✓")} removed ${plist}`);
  } else {
    console.log(dim("no launchd agent to remove"));
  }
  return 0;
}

function cmdSecret(): number {
  if (!requireEventsConfigured()) return 1;
  const events = config.github.events!;
  ensureEventsDir();
  const secret = ensureSecret();
  if (!secret) return 1;
  console.log(`${green("✓")} webhook secret ${secret.statusLine}`);
  printWebhookSetup(events.host, events.port, secretDisplay(secret));
  return 0;
}

export async function run(argv: string[]): Promise<number> {
  const [sub] = argv;
  switch (sub) {
    case "serve":
      return runDaemonForeground();
    case "status":
      return cmdStatus();
    case "install":
      return cmdInstall();
    case "uninstall":
      return cmdUninstall();
    case "start":
      return requireEventsConfigured() ? launchctl("load") : 1;
    case "stop":
      return launchctl("unload");
    case "secret":
      return cmdSecret();
    case undefined:
    case "--help":
    case "-h":
      console.log(USAGE);
      return sub ? 0 : 2;
    default:
      console.error(red(`unknown events subcommand: ${sub}\n`));
      console.error(USAGE);
      return 2;
  }
}
