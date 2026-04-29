# wt

Terminal UI for keeping multiple git worktrees in flight at once. Each row shows live status, PR state, preview deployment, issue link, and Claude Code session activity for one worktree, so the whole pile of in-progress work is visible on one screen. The details pane can also pull an AI-generated title and 1–3 sentence description for each branch from a local OpenAI-compatible LLM endpoint (LM Studio etc.).

![screenshot](docs/screenshot.png)

## Requirements

**Required**

- [Bun](https://bun.sh) — runtime.
- `git` — worktree mechanics.
- A [Nerd Font](https://www.nerdfonts.com/) — the TUI uses Nerd Font glyphs for status, PRs, checks, merge-queue position, etc. Without one, those cells render as tofu.
- macOS — `open` and `pbcopy` are assumed for URL/clipboard handling.

**Optional, per integration**

- `gh` (GitHub CLI, authenticated) — needed for the PR row (PR state, checks, review, merge queue). Also used to derive the repo's `nameWithOwner`.
- `aws` CLI with a profile that can read your SST state bucket — needed when `[deploy.sst]` is configured (drives the stage row + `wt stages`).
- `zed` CLI — needed for `wt open` and the `o` keybinding.
- Linear — no CLI; the integration only constructs URLs from issue IDs in your branch slug.
- Claude Code — no CLI; the integration reads `~/.claude/projects/*` directly to surface live session state.
- An OpenAI-compatible LLM endpoint (LM Studio, Ollama with the OpenAI bridge, llama.cpp's server, etc.) — needed for the AI title + description in the details pane. `wt` runs a single graceful-degradation diff through `/v1/chat/completions`; results are content-addressed so identical diffs (across rebase / amend / branch rename) share a cached summary.

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

[ai]
endpoint         = "http://127.0.0.1:1234"   # OpenAI-compatible /v1
model            = "gemma-3-e4b-it-mlx"      # whatever the endpoint calls it
max_input_tokens = 8000                       # optional; default 8000
timeout_ms       = 120000                     # optional; default 120000 (local LLMs cold-start slowly)
```

The loader prints every missing or malformed field at once. See [`src/core/config.ts`](src/core/config.ts) for the full schema, defaults, and the row-ordering knob (`[ui] rows`).

## Usage

`wt` with no arguments launches the TUI. Subcommands (`ls`, `new`, `rm`, `clean`, `doctor`, `stages`, `logs`, `open`) run one-shot CLI ops — see `wt --help` and `wt <cmd> --help`.

## Logs

Every action (and every error) goes to a daily file at `~/.cache/wt/logs/app/wt-YYYY-MM-DD.log` for post-hoc debugging — a strict superset of what the activity pane shows. Files older than 14 days are pruned automatically. Per-worktree destroy logs are still at `~/.cache/wt/logs/<slug>-*.log` and `wt logs <slug>` tails the latest one.
