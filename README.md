# wt

Terminal UI for keeping multiple git worktrees in flight at once. Each row shows live status, PR state, preview deployment, issue link, and coding-agent session activity (Claude Code, Codex, OpenCode) for one worktree, so the whole pile of in-progress work is visible on one screen. The details pane can also pull an AI-generated title and 1–3 sentence description for each branch from a local OpenAI-compatible LLM endpoint or Google's Gemini API.

![screenshot](docs/screenshot.png)

## Requirements

**Required**

- [Bun](https://bun.sh) — runtime.
- `git` — worktree mechanics.
- A [Nerd Font](https://www.nerdfonts.com/) — the TUI uses Nerd Font glyphs for status, PRs, checks, merge-queue position, etc. Without one, those cells render as tofu.
- macOS — `open` and `pbcopy` are assumed for URL/clipboard handling.

**Optional, per integration**

- `gh` (GitHub CLI, authenticated) — needed for the PR row (PR state, checks, review, merge queue, auto-merge state, suggested + requested reviewers) and for the in-TUI PR actions (arm/disable auto-merge, mark draft ready, edit reviewers). Also used to derive the repo's `nameWithOwner`.
- `aws` CLI with a profile that can read your SST state bucket — needed when `[deploy.sst]` is configured (drives the stage row + `wt stages`).
- `zed` CLI — needed for `wt open` and the `o` keybinding.
- [`hunk`](https://github.com/modem-dev/hunk) (`npm i -g hunkdiff`) — needed for the default F11 diff command (`hunk diff {{base}} --watch`). Override `[diff].command` in `config.toml` if you'd rather use `gitu`, `lazygit`, etc.
- Linear — no CLI; the integration constructs issue URLs from branch slugs and can open PRs in Linear Reviews.
- Coding-agent sessions — `wt` *detects* live sessions by reading each agent's local files (Claude Code `~/.claude/projects/*`, Codex `~/.codex/sessions/*`, OpenCode `~/.local/share/opencode`), so no CLI is needed just to surface state. To *spawn* sessions from the TUI you need that agent's CLI on PATH (`claude`, `codex`, `opencode`). Claude is the most complete integration (busy/idle state, AI summaries); Codex and OpenCode are partial today.
- An AI provider — optional; needed for the generated title + description in the details pane. `wt` supports OpenAI-compatible endpoints and Google's Gemini API. Results are content-addressed so identical diffs (across rebase / amend / branch rename) share a cached summary.

## Install

```sh
git clone https://github.com/micthiesen/wt.git ~/.wt
cd ~/.wt && bun install
```

Add to your shell rc:

```sh
alias wt='~/.wt/bin/wt'
```

## Config

`wt` refuses to start without a config. Create `~/.config/wt/config.toml`:

```toml
[paths]
main_clone     = "~/Code/your-repo"
worktree_root  = "~/Code/your-repo-wt"

[branch]
prefix = "yourname"   # branches you create get `yourname/<id>-<slug>`

# Optional integrations — omit any section to disable that integration.

[stage]
domain = "preview.example.com"   # used to build per-stage preview URLs

[deploy.sst]
state_bucket = "sst-state-xxxxxxxx"
state_prefix = "app/app/"
aws_profile  = "default"

[issue_tracker.linear]
workspace = "your-workspace"

[github]
pr_target = "linear"   # optional: `p` opens PRs in Linear Reviews instead of GitHub

[ai]
endpoint         = "http://127.0.0.1:1234"   # OpenAI-compatible /v1
model            = "gemma-3-e4b-it-mlx"      # whatever the endpoint calls it
max_input_tokens = 8000                       # optional; default 8000
timeout_ms       = 120000                     # optional; default 120000 (local LLMs cold-start slowly)

# Or use Gemini instead:
#
# [ai]
# provider         = "gemini"
# model            = "gemini-3.5-flash"
# api_key_env      = "GEMINI_API_KEY"
# max_input_tokens = 8000
# timeout_ms       = 120000

[github.events]                               # optional; push PR/CI updates instead of polling
port        = 8765                            # port the webhook daemon listens on
host        = "127.0.0.1"                     # default loopback; set to a LAN IP / 0.0.0.0 only if a separate proxy box must reach it
secret_file = "~/.config/wt/gh-webhook-secret"  # HMAC secret (or inline `secret = "…"`)
```

The loader prints every missing or malformed field at once. See [`src/core/config.ts`](src/core/config.ts) for the full schema and defaults, including the row-ordering knob (`[ui] rows`), the `[[actions]]` tables that populate the `!` action menu, and the opt-in `[[automations]]` tables that fire those actions (or the built-in clean/restack flows) automatically off PR and stack state — once per failure instance, only after the worktree settles, with a per-worktree circuit breaker. `A` pauses all automations; `Ctrl+A` pauses the selected worktree, or its whole stack when it's a slice. Both persist across restarts.

## Usage

`wt` with no arguments launches the TUI. Press `?` inside for the full keymap and glyph legend, and `/` to filter that help. The `p` key opens a PR in `[github].pr_target`; `g p` and `l p` explicitly open the same PR in GitHub or Linear Reviews.

Subcommands run one-shot CLI ops:

| command | what it does |
|---|---|
| `ls` | list worktrees (status, PR, stage) |
| `new` / `rm` | create / remove a worktree |
| `clean` | remove merged or upstream-gone worktrees |
| `doctor` | report health of one or all worktrees |
| `open` | open a worktree in Zed |
| `stages` | list SST stages, optionally clean orphans |
| `logs` | tail a worktree's destroy log |
| `base` | show / set / clear a worktree's recorded fork base |
| `size` | production-LOC + file count for a diff |
| `stack` | materialize / inspect / restack a stacked-PR manifest |
| `skills` | install wt's bundled workflow skills into an agent |
| `events` | manage the optional GitHub webhook daemon |
| `claude` | drive a worktree's Claude Code session (send / ls / kill) |

Run `wt <cmd> --help` for per-command options.

## GitHub webhooks (optional)

By default the PR / checks / merge-queue badges refresh by polling `gh` on a 60s timer. Add a `[github.events]` section and run a small local daemon to have GitHub *push* updates instead — faster badges, far fewer polls, and a warm snapshot so a freshly-opened TUI already shows current state.

It's a plain repo webhook (no GitHub App). The daemon listens on `[github.events].host` (default loopback); map a public HTTPS URL to it however you route traffic into your network. If a reverse proxy on a *different* host has to reach this machine, set `host` to a LAN IP or `0.0.0.0` — the HMAC secret is then the only auth boundary, so keep it on a trusted network.

```sh
wt events install     # writes a launchd agent + generates the HMAC secret
wt events start       # load the daemon
wt events status      # liveness, last delivery, snapshot age
```

`install` prints the exact values to paste into the repo's **Settings → Webhooks** (Payload URL, content type `application/json`, the secret, and the event checklist: `pull_request`, `pull_request_review`, `pull_request_review_thread`, `check_suite`, `check_run`, `status`, `merge_group`, `push`). The daemon verifies GitHub's `X-Hub-Signature-256` HMAC and only ever runs the same read-only `gh` fetch the TUI already uses — webhook payloads are a refresh *signal*, never a data source. Omit the section entirely and nothing changes (poll-only).

## Stacked PRs (optional)

`wt` carries an opinionated small-stacked-PR workflow: implement a feature holistically on one branch, then carve it into a stack of small draft PRs that review independently. `wt size` reports a diff's production LOC + file count (the budget you split against), and `wt stack` materializes and maintains the stack manifest — worktrees, draft PRs, and restacking after a parent merges. The day-to-day driver is a set of bundled agent skills (`/split`, `/restack`, plus a `wt` reference skill) you install with:

```sh
wt skills install --harness claude     # or codex / opencode
```

## Logs

Every action (and every error) goes to a daily file at `~/.cache/wt/logs/app/wt-YYYY-MM-DD.log` for post-hoc debugging — a strict superset of what the activity pane shows. Files older than 14 days are pruned automatically. Per-worktree destroy logs are still at `~/.cache/wt/logs/<slug>-*.log` and `wt logs <slug>` tails the latest one.
