# Configuration reference

`wt` reads a single TOML file, resolved in this order:

1. `$WT_CONFIG` (explicit path override)
2. `$XDG_CONFIG_HOME/wt/config.toml`
3. `~/.config/wt/config.toml`

The loader is fail-fast: it validates everything at startup and exits with **one aggregated error message** listing every missing or malformed field. There is no hot reload — edits require restarting `wt`.

Only three fields are required. Everything else has a generic default or is an optional integration that turns on when its section is present.

```toml
[paths]
main_clone    = "~/Code/your-repo"
worktree_root = "~/Code/your-repo-wt"

[branch]
prefix = "yourname"
```

The source of truth for the schema is [`src/core/config.ts`](../src/core/config.ts).

## `[paths]`

| key | required | default | meaning |
|---|---|---|---|
| `main_clone` | **yes** | — | The primary clone of the repo the worktrees belong to. |
| `worktree_root` | **yes** | — | Directory where worktrees are created (`<worktree_root>/<slug>`). |
| `log_dir` | no | `~/.cache/wt/logs` | Per-worktree destroy logs live here; daily structured app logs go to the derived `<log_dir>/app` subdirectory. |
| `lock_dir` | no | `~/.cache/wt/locks` | Per-slug operation locks (what drives the "setting up…" busy state). |
| `cache_db` | no | `~/.cache/wt/cache.sqlite` | SQLite blob persisting the TanStack Query cache between runs. |
| `wezterm_cli` | no | macOS: `/Applications/WezTerm.app/Contents/MacOS/wezterm`; elsewhere: `wezterm` from `PATH` | WezTerm CLI executable used to set the tab title to `wt` when `WEZTERM_PANE` is present. Supports `~` expansion. |

## `[branch]`

| key | required | default | meaning |
|---|---|---|---|
| `prefix` | **yes** | — | Branches you create become `<prefix>/<id>-<slug>`. Also seeds the `[stage]` defaults. |
| `base` | no | `"main"` | Trunk branch name. Diff bases, sync counts, merge detection all resolve against `origin/<base>`. |
| `id_pattern` | no | `"^[a-z]+-(\\d+)(?:-|$)"` | Regex (no flags) matching an issue ID at the start of a slug. The default matches Linear/Jira/Shortcut-style ids (`eng-1234`, `inf-99`). |
| `slug_max_len` | no | `50` | Slugs generated from issue titles are truncated to this length. |

## `[stage]`

Preview-stage naming, used by the SST integration and stage URLs.

| key | required | default | meaning |
|---|---|---|---|
| `prefix` | no | `"<branch.prefix>-"` | Every per-worktree stage is `<prefix><slug-derived-name>`; the prefix guard is what keeps `wt` from ever touching stages it doesn't own. |
| `default_personal` | no | `branch.prefix` | Stage name reserved for your personal environment; excluded from orphan cleanup. |
| `domain` | no | *(unset)* | Public domain for building per-stage preview URLs (`https://<stage>.<domain>`). Unset ⇒ no stage URLs are constructed. |

## `[lifecycle]`

| key | required | default | meaning |
|---|---|---|---|
| `env_files_to_copy` | no | `[".env"]` | Files copied from the main clone into each new worktree during setup. |

## `[deploy.sst]` — optional integration

Omit the whole section to disable SST awareness (the stage row, `wt stages`, deploy detection). When present, all three keys are required.

| key | required | default | meaning |
|---|---|---|---|
| `state_bucket` | **yes** | — | S3 bucket holding SST's Pulumi state. |
| `state_prefix` | **yes** | — | Key prefix within the bucket (SST v3 convention: `<state_prefix><stage>.json`). |
| `aws_profile` | **yes** | — | AWS CLI profile with read access to the state bucket. |
| `auto_regen_paths` | no | `["sst-env.d.ts"]` | Files in the main clone that `sst` runs regenerate; restored before fetches so they never show as dirt. |

## `[issue_tracker.linear]` — optional integration

Omit to disable the Linear row and Linear PR deep-links.

| key | required | default | meaning |
|---|---|---|---|
| `workspace` | **yes** | — | Linear workspace slug; issue URLs are constructed as `https://linear.app/<workspace>/issue/<id>` from the branch slug (no API calls, no token). |

## `[ai]` — optional integration

Omit to disable the AI-generated title/brief/description in the details pane. Two providers:

```toml
# OpenAI-compatible endpoint (LM Studio, Ollama, llama.cpp, an actual OpenAI-style server…)
[ai]
endpoint = "http://127.0.0.1:1234"   # required for provider = "openai"
model    = "gemma-3-e4b-it-mlx"

# or Gemini
[ai]
provider    = "gemini"
model       = "gemini-3.5-flash"
api_key_env = "GEMINI_API_KEY"       # required for provider = "gemini"
```

| key | required | default | meaning |
|---|---|---|---|
| `provider` | no | `"openai"` | `"openai"` or `"gemini"`. |
| `model` | **yes** | — | Model id as the provider names it. |
| `endpoint` | openai: **yes** | gemini: `https://generativelanguage.googleapis.com/v1beta` | Base URL, no trailing slash. |
| `api_key_env` | gemini: **yes** | — | Name of the environment variable holding the Gemini API key. |
| `max_input_tokens` | no | `8000` | Soft prompt budget; diff hunks are dropped largest-first to stay under it. |
| `timeout_ms` | no | `120000` | Per-request timeout. Generous by default because local LLMs cold-start slowly. |

Summaries are content-addressed by a hash of the diff, so identical diffs (across rebases, amends, branch renames) reuse the cached result.

## `[github]`

| key | required | default | meaning |
|---|---|---|---|
| `ignored_checks` | no | `[]` | Glob patterns (case-insensitive, `*` wildcard only) matched against check names; matching contexts are dropped from the PR checks rollup so non-CI bots (CodeRabbit etc.) don't flip the badge. |
| `default_reviewer` | no | *(unset)* | GitHub login requested by the `E` ("ship it") chord (mark ready + request reviewer + arm auto-merge). Unset disables the reviewer leg. |
| `pr_target` | no | `"github"` | Where `p` opens PRs: `"github"` keeps GitHub URLs, `"linear"` rewrites them to Linear Reviews deep-links. `g p` / `l p` always open GitHub / Linear explicitly. |

## `[github.events]` — optional webhook daemon

Omit for classic poll-only behavior. When present, the `wt events` daemon accepts GitHub webhook deliveries and pushes PR/check updates to the TUI instead of waiting on the poll. Setup walkthrough: [github-events.md](github-events.md).

| key | required | default | meaning |
|---|---|---|---|
| `port` | no | `8765` | Port the daemon listens on. |
| `host` | no | `"127.0.0.1"` | Bind address. Keep loopback when the public URL terminates on this machine; set a LAN IP / `0.0.0.0` only if a separate proxy box must reach it (the HMAC secret is then the only auth boundary). |
| `secret` | no | *(unset)* | Inline HMAC secret for `X-Hub-Signature-256` verification. Prefer `secret_file`; inline wins if both are set. |
| `secret_file` | no | *(unset)* | Path to a file holding the HMAC secret (home-expanded). `wt events install` generates one. |
| `backstop_poll_ms` | no | `600000` | github-query staleness bound while events are configured — only matters if the daemon dies or a delivery is dropped. |

## `[diff]`

| key | required | default | meaning |
|---|---|---|---|
| `command` | no | `"revdiff --vim-motion --compact {{base}}"` | Shell command F11 launches inside the selected worktree (via `$SHELL -lc`, so pipes and aliases work). `{{base}}` substitutes the worktree's resolved diff base: `origin/<trunk>` normally, the parent branch for stacked worktrees. Swap in `gitu`, `lazygit`, `tig status`, a `delta` pipe, or any script. Commands using `{{base}}` get their session killed when the resolved base changes (PR base flip, stack reroot) so the next F11 reopens against the right ref. |

## `[ui]`

| key | required | default | meaning |
|---|---|---|---|
| `rows` | no | `["branch", "base", "linear", "stage", "pr", "claude", "git"]` | Detail-pane row order. Available ids: `branch`, `base`, `path`, `linear`, `stage`, `pr`, `claude`, `git`. Unknown ids are ignored; omitted ones are hidden. A row also hides itself when its integration isn't configured (e.g. `linear` without `[issue_tracker.linear]`). The rebase state (restacking / mid-rebase / conflict + files) isn't a row — it renders as a fixed block below the rows, above the AI summary. |

## `[[actions]]` — the `!` menu

Pre-built actions surfaced by the `!` picker (and available as automation targets). Two kinds, distinguished by which field you set:

- **Prompt actions** (`prompt = "…"`): run the worktree's primary coding agent. Default delivery is a tracked headless run (`claude -p` / `codex exec` / `opencode run`); `target = "session"` instead injects the prompt into the live F12 session (fire-and-forget: no completion signal, so `affects` won't auto-refresh).
- **Shell actions** (`shell = "…"`): run `$SHELL -lc <shell>` in the worktree path; Enter launches directly with no edit step.

**Replacement semantics:** when `[[actions]]` is absent, two built-ins apply (`rebase-main` "Rebase on base", `address-review` "Address PR review"). The moment you define *any* entry, your list fully replaces the defaults — to drop one default, list everything you keep.

```toml
[[actions]]
id       = "deploy"
name     = "Deploy preview"
shell    = "pnpm deploy:local --stage {{stage}}"
group    = "deploy"        # optional picker section header
key      = "d"             # optional quick-pick letter (auto-derived when omitted)
requires = ["deployed"]

[[actions]]
id         = "fix-ci"
name       = "Fix failing CI"
prompt     = "CI is failing on the PR for this branch ({{pr}}). Investigate and fix, then push."
target     = "headless"    # or "session"
affects    = ["git", "github"]
arg_prompt = "extra context"      # optional: collect a per-launch {{arg}} value
label_extract = "^Fixed: (.+)$"   # optional: regex labeling history entries from run output
```

Fields:

| key | applies to | default | meaning |
|---|---|---|---|
| `id` | both | — (required) | Unique id; what `[[automations]].run` references. |
| `name` | both | — (required) | Picker label. |
| `prompt` / `shell` | — | — | Exactly one must be set; picks the kind. |
| `target` | prompt only | `"headless"` | `"headless"` or `"session"` (see above). |
| `affects` | both | prompt: `["git", "github"]`, shell: `[]` | State domains the action mutates; the matching caches are refreshed when the run exits. Tags: `git`, `github`. Explicit `[]` opts out. |
| `requires` | both | `[]` | Preconditions; unmet entries gray out in the picker with the reason. Tags: `pr` (any PR exists), `pr.ready` (open non-draft PR), `deployed` (this worktree's SST stage is live). |
| `key` | both | auto-derived | Single-char quick-pick letter in the `!` menu. |
| `group` | both | ungrouped | Section label; same-group actions cluster under one header. |
| `arg_prompt` | both | *(unset)* | Label for a per-launch value prompt. Picking the action first shows recent values (from `~/.cache/wt/action-history.json`) plus a "new…" input; the value substitutes `{{arg}}`. |
| `label_extract` | both | *(unset)* | Regex (source string, no flags) scanned against the run's output; the last per-line match (capture group 1, or the full match) becomes the history label for the `{{arg}}` value. |

**Template variables** (`{{var}}`, unknown vars pass through so typos are visible): `{{base}}` resolved diff base, `{{base_branch}}` parent branch or trunk, `{{branch}}`, `{{slug}}`, `{{cwd}}` worktree path, `{{pr}}` PR number or empty, `{{stage}}` the worktree's SST stage, `{{skill_prefix}}` the harness's skill-invocation prefix (`/` for Claude Code, `$` for Codex/OpenCode — write `{{skill_prefix}}restack` to invoke a skill portably), and `{{arg}}` for the collected `arg_prompt` value.

## `[[automations]]` — optional, strictly opt-in

Rules that fire actions (or built-in flows) automatically off PR and stack state ([stacked-prs.md](stacked-prs.md)). No defaults ship; an absent section means nothing is automated. Deep dive on the semantics (fire keys, settle windows, circuit breaker): [automations.md](automations.md).

```toml
[[automations]]
id  = "auto-restack"
on  = "stack.parent_merged"
run = "builtin:restack"

[[automations]]
id               = "auto-fix-ci"
on               = "pr.checks.failed"
run              = "fix-ci"          # an [[actions]] id
busy             = "queue"           # or "skip"
cooldown_minutes = 30
settle_seconds   = 300
```

| key | required | default | meaning |
|---|---|---|---|
| `id` | **yes** | — | Unique rule id (used in fire-key bookkeeping and logs). |
| `on` | **yes** | — | Trigger: `pr.checks.failed`, `rabbit.unresolved` (CodeRabbit threads), `review.changes_requested`, `pr.conflict`, `wt.merged` (a non-stacked worktree landed), `stack.parent_merged` (a stack member's parent landed). |
| `run` | **yes** | — | An `[[actions]]` id, or a builtin: `builtin:restack` (only valid with `stack.parent_merged`), `builtin:clean` (any single-worktree trigger). |
| `busy` | no | `"queue"` | When the worktree isn't quiescent at delivery time: `queue` holds the intent until it settles, `skip` drops it. |
| `cooldown_minutes` | no | *(none)* | Minimum minutes between dispatches per (rule, worktree). |
| `settle_seconds` | no | `120` (merge triggers: `10`) | Quiescence window: the condition must hold and the worktree be edit-free this long before delivery. Doubles as your cancellation grace period. |

At runtime, `A` pauses all automations and `Ctrl+A` pauses the selected worktree (or its whole stack); both persist across restarts.
