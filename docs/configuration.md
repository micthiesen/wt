# Configuration reference

`wt` first reads a user TOML file, resolved in this order:

1. `$WT_CONFIG` (explicit path override)
2. `$XDG_CONFIG_HOME/wt/config.toml`
3. `~/.config/wt/config.toml`

It then searches from the current directory upward for the nearest `.wt.toml` and recursively merges that repository config over the user config. This lets one user config hold personal defaults while each repository supplies its own clone, worktree root, trunk branch, integrations, actions, and other overrides. Run `wt` from within the repository you want to manage.

`wt init [directory]` creates that repository file without requiring an
already-valid wt config. It derives a readable repository id from the config
directory (`~/dev/cz/cozee-dev` → `dev-cz-cozee-dev`). That id partitions the
global durable state database and names the default cache directory and tmux
socket, so existing and future worktrees in different repositories cannot
reap or rearrange one another.

TOML tables merge by key. Scalar values and arrays replace the user value completely, so a repository's `[[actions]]` or `[[automations]]` list is authoritative when present. `$WT_CONFIG` selects a different user config; it does not disable repository overrides. Internally, `wt` carries the selected repository file into child processes with `$WT_REPO_CONFIG`, which also provides an explicit repository-config path for scripts that cannot preserve the invocation directory.

For example, shared personal defaults can live in `~/.config/wt/config.toml`:

```toml
[branch]
prefix = "yourname"

[ui]
sort = "status"
```

Each repository can then provide the required paths and any project-specific settings in `.wt.toml`:

```toml
[paths]
main_clone    = "~/Code/project-a"
worktree_root = "~/Code/project-a-wt"

[branch]
base = "develop"
```

The repository file may be committed when its values are useful to every contributor, or ignored when it contains machine-specific paths.

The loader is fail-fast: it validates the merged result at startup and exits with **one aggregated error message** listing every missing or malformed field. A user config file must exist, but required fields may come from either layer. When `[paths]` is missing and no `.wt.toml` was found from the current directory upward, the error names both candidate homes — run `wt` inside the repo, or keep `[paths]` in the user config as the fallback. There is no hot reload; edits require restarting `wt`.

Only three fields are required in the merged result. Everything else has a generic default or is an optional integration that turns on when its section is present.

```toml
[paths]
main_clone    = "~/Code/your-repo"
worktree_root = "~/Code/your-repo-wt"

[branch]
prefix = "yourname"
```

The source of truth for the schema is [`src/core/config.ts`](../src/core/config.ts).

## `[instance]`

| key | required | default | meaning |
|---|---|---|---|
| `role` | no | `"controller"` | `controller` owns the TUI/layout. `worker` accepts SSH execution and agent-status commands but disables the TUI and `wt section`. |

Use the same wt installation on both machines. A remote execution host opts
into worker mode in its user config:

```toml
[instance]
role = "worker"
```

The controller verifies this role, the worker protocol version, and the
running build during its SSH handshake. Protocol compatibility controls
whether commands may run; a differing build remains usable but is shown as a
version warning so development snapshots can be synchronized deliberately.

## `[paths]`

| key | required | default | meaning |
|---|---|---|---|
| `main_clone` | **yes** | — | The primary clone of the repo the worktrees belong to. |
| `worktree_root` | **yes** | — | Directory where worktrees are created (`<worktree_root>/<slug>`). |
| `log_dir` | no | `<cache root>/logs` | Per-worktree destroy logs live here; daily structured app logs go to the derived `<log_dir>/app` subdirectory. |
| `lock_dir` | no | `<cache root>/locks` | Per-slug operation locks (what drives the "setting up…" busy state). |
| `cache_db` | no | repository with a `.wt.toml`: `~/.cache/wt/<repo-id>/cache.sqlite`; otherwise `~/.cache/wt/cache.sqlite` | Disposable TanStack Query cache. Its directory (the **cache root**) also anchors rebuildable/runtime files: session registries, automation delivery files, manager reports, logs, locks, generated `tmux.conf`, message sockets, and shims. Durable section/status/archive state is not stored here. |
| `state_db` | no | repository with a `.wt.toml`: `~/.local/state/wt/wt.sqlite`; user-config-only compatibility mode: `<cache root>/wt.sqlite` | Authoritative SQLite state shared by local repositories and partitioned by `repo_id`. Normally leave this unset. The canonical repository path stored with each id is a collision guard. |
| `wezterm_cli` | no | macOS: `/Applications/WezTerm.app/Contents/MacOS/wezterm`; elsewhere: `wezterm` from `PATH` | WezTerm CLI executable used to set the tab title to `wt` when `WEZTERM_PANE` is present. Supports `~` expansion. |
| `dotfiles` | no | `~/.dotfiles` | Repo behind the general-purpose config session (`/` and its `\` palette). **The slot hides itself entirely when the directory doesn't exist** — no footer button, and `/` / `\` fall through — so a machine without a dotfiles repo isn't offered a key that can only cold-start a harness in a missing directory. |

### Repository identity is a property of the repository, not of your shell

Every default above keys on `<repo-id>`, and that id comes from the repository
the command acts on — `paths.main_clone`, or the directory of a `.wt.toml`
outside `worktree_root`. It deliberately does **not** come from whichever
`.wt.toml` happens to be nearest the working directory. A worktree carries a
copy of its repository's `.wt.toml` (the backends clone the whole tree) and a
shell outside the repository finds none at all, so a cwd-derived id gives one
repository several namespaces: several state databases, several cache roots and
several tmux servers, each internally consistent and blind to the others. A
status asserted in a worktree is then invisible everywhere else, `wt manager
send` cold-starts a manager on a tmux server the TUI never reads, and a tracker
id set in the TUI is invisible to the session it names.

`wt` inside a worktree, in the main clone, and from an unrelated directory all
resolve the same configuration, byte for byte.

> **Upgrade note (Aug 2026):** `wt state migrate` imports the current repository's attributable records from the former shared `~/.cache/wt/state.json` and `archive.json`, writes timestamped backups, and prunes only records successfully imported. It also adopts records an earlier build filed under a per-worktree namespace or wrote into a second state database, and carries the non-rebuildable runtime files (the `automations.json` fire ledger, `harness.json`, and the harness session-name registries) into this repository's cache root. Stranded sources are read, never written or deleted, and current values win every conflict. It is idempotent; use `--keep-legacy` for a copy-only first pass or `--from <dir>` for a relocated legacy cache.

### Running a second isolated instance

Repository `.wt.toml` files are isolated automatically: their path-derived id partitions durable database rows and derives distinct cache roots and tmux sockets. Explicit overrides remain useful for test/dogfood instances:

```toml
[paths]
main_clone    = "~/Code/other-repo"
worktree_root = "~/Code/other-repo-wt"
cache_db      = "~/.cache/wt-other/cache.sqlite"

[tmux]
socket = "wt-other"
```

Pick it with `WT_CONFIG=/path/to/config.toml` (or let a repo `.wt.toml` supply the same keys). `WT_TMUX_SOCKET` still overrides `[tmux] socket` and still propagates into every session the tmux server spawns — that precedence is deliberate, since a `wt` call made *inside* an already-running session must resolve the socket it was started under regardless of what config it later reads. Putting the socket in config is what removes the need to wrap every entry point in a shell function that exports the variable.

## `[tmux]`

| key | required | default | meaning |
|---|---|---|---|
| `socket` | no | repository with a `.wt.toml`: `"wt-<repo-id>"`; otherwise `"wt"` | Socket name (`tmux -L <socket>`) for the wt-private tmux server that hosts every agent, shell, diff, dev-server and action session. `WT_TMUX_SOCKET` wins when set. |
| `terminal_config` | no | built-in terminal preamble | Literal tmux configuration replacing the entire terminal preamble, including mouse, clipboard, hyperlinks, terminal features, extended keys, alternate-screen, status and title settings. Omit to follow wt defaults; set to a TOML multiline literal string to pin or customize every terminal setting. An empty string deliberately removes the preamble. wt still appends the observed terminal palette and F10/F11/F12 navigation bindings. Applies when the server starts or an interactive attach reloads its config; terminal capability changes can require detaching and reattaching. |

## `[codex]`

These settings apply only to interactive Codex sessions launched through wt,
including fresh and resumed worktree, main, and manager sessions. Other Codex
options continue to come from Codex's own configuration. Running sessions keep
their launch settings until they exit and resume; reattaching does not change them.

| key | required | default | meaning |
|---|---|---|---|
| `animations` | no | `false` | Enable Codex TUI animations. The default avoids animated-composer artifacts with affected tmux versions. Set `true` when animations work with your terminal setup. |
| `alternate_screen` | no | `"always"` | Codex TUI alternate-screen policy: `"always"`, `"auto"`, or `"never"`. The default keeps the full-height viewport stable under tmux. |

```toml
[codex]
animations = true
alternate_screen = "always"
```

## `[branch]`

| key | required | default | meaning |
|---|---|---|---|
| `prefix` | **yes** | — | Branches you create become `<prefix>/<id>-<slug>`. Also seeds the `[stage]` defaults. |
| `base` | no | `"main"` | Trunk branch name. Diff bases, sync counts, merge detection all resolve against `origin/<base>`. |
| `id_pattern` | no | `"^[a-z]+-(\\d+)(?:-|$)"` | Regex (no flags) matching an issue ID at the start of a slug. The default matches Linear/Jira/Shortcut-style ids (`eng-1234`, `inf-99`). |
| `slug_max_len` | no | `50` | Slugs generated from issue titles are truncated to this length. |
| `keep_fresh` | no | `[]` | Extra local branches the **main clone** keeps current, alongside `base`. See below. |

### `keep_fresh` — reference branches nobody forks from

```toml
[branch]
base       = "staging"
keep_fresh = ["main"]
```

When you fork from `staging`, `main` is still the branch you check
against to see what is in production — and nothing was keeping it
current. `git fetch --prune` (which `fetchOrigin` runs every few
minutes) moves every `origin/<branch>`, but only the local head named by
`base` was being advanced, so `git log main` in the main clone answered
about the last time somebody typed `git fetch origin main:main` by hand.

Each named branch is advanced in the main clone on every fetch:

- Not checked out → `git branch --force <branch> origin/<branch>`, which
  also **creates** it when the clone never had it. A `keep_fresh` entry
  is a request for a local head that tracks origin, so absent is the
  condition it exists to fix. (`base` keeps its historical
  skip-if-absent behaviour — it is a branch wt assumes the clone already
  manages.) `branch --force` rather than a raw ref write because it
  refuses to move a branch checked out in another worktree.
- Checked out and clean → `git merge --ff-only`.
- Checked out and dirty → skipped. `origin/<branch>` is fresh either
  way, and that is what the semantic checks read.

**Fast-forward only, in every branch of it.** A local head that has
diverged is left exactly where it is, with a warning in the app log.
This runs unattended; the one thing it must never do is pick a winner.

Only the main clone is touched. Under the `rift` backend each worktree
is an independent clone with its own refs — see
[backends.md](backends.md#stale-remote-tracking-refs).

## `[remote]` — optional SSH worktree host

Configure a second machine whose own `wt` installation, clone, config, and
worktree root remain authoritative for execution. Set `[instance] role =
"worker"` on that machine. The controller TUI polls the worker's worktree
summaries and renders them in the same sections as local worktrees, with a
small remote indicator on each row. `Ctrl+N` forwards the normal `wt new`
lifecycle over SSH; F10/F11/F12 on one of those rows attach to that
worktree's remote tmux shell, diff, or AI session. The `!` picker is identical
for local and remote rows: requirements/templates use one normalized row model,
while tracked commands, custom prompts, dev controls/logs, and cancellation run
against the selected checkout through SSH. Ordinary `n` / `N`
continue to create locally.

The last successful remote inventory is persisted with the rest of wt's query
cache. If the host sleeps or becomes unreachable, those rows remain visible as
last-known state and are marked `host unavailable`; they are not interpreted as
deleted worktrees. The inventory carries agent work-status assertions and the
worker's full dev-server health snapshot, so the controller renders the same
status dot, live bolt, URL, startup/restart/queue state, and crash diagnostics
as it does for local rows.

The controller fetches this through the versioned hidden `_snapshot` contract,
not by treating the human-facing `wt ls --json` schema as an RPC. Local and
remote adapters therefore produce the same nested application state; SSH
location changes execution routing and adds the server glyph, not feature or
badge behavior.

Section assignment, ordering, fold state, and archiving all belong to the
controller. A worker-reported section is ignored; a newly discovered remote row
starts in the controller's Inbox. Moving it records a host-qualified (`host` +
`slug`) layout entry in the controller's SQLite state and never mutates the
worker checkout or database. Archiving uses the same location-aware identity.
Filesystem, Git, tmux, and destructive operations continue to execute remotely.
The schema currently accepts one `[remote]`, but stored identity and query
caches are host-qualified; future multiple-remote support will not conflate
same-named worktrees or depend on display labels being unique.

```toml
[remote]
host = "cachy"                 # SSH host or ~/.ssh/config alias
label = "cachy"                # optional; defaults to host
wt_path = "~/.wt/bin/wt"       # optional
```

| key | required | default | meaning |
|---|---|---|---|
| `host` | **yes** | — | SSH destination or alias used by `ssh`. |
| `label` | no | `host` | Short name in the prompt, event log, and remote WezTerm tab title. |
| `wt_path` | no | `~/.wt/bin/wt` | Remote executable. The `~/` prefix expands in the remote account. |

The remote machine needs its own `~/.config/wt/config.toml`, including
`[instance] role = "worker"`; do not point the local process at a mounted
remote filesystem. `wt remote agent start <slug>` first runs a non-interactive
remote `wt skills sync --yes`, provisioning missing/current bundled skills and
the managed instructions block before any harness command is typed. Personal
or modified copies keep the normal skills-sync protection and are not
overwritten. The worker then fails closed if the selected harness still cannot
resolve `start`. Every spawned harness also receives the configured remote
`wt_path` launcher's directory at the front of `PATH`, so the skill can call
`wt status` even when that directory is absent from the SSH login PATH.

`wt remote [args…]` remains a
diagnostic/admin escape hatch, but normal work happens from the unified local
Inbox.

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
| `copy_globs` | no | `[]` | Glob patterns resolved relative to the main clone and copied into each new worktree with their paths preserved. Dotfiles are included except root `.git` metadata, existing destinations are not overwritten, and patterns must be relative without `..` segments. Example: `[".agents/**"]`. |
| `install_command` | no | *(auto-detect)* | Dependency install run in a fresh `git-worktree` checkout, via `$SHELL -lc`. Unset ⇒ detect the package manager from the checkout's lockfile (`bun.lock`/`bun.lockb` → `bun install`, `pnpm-lock.yaml` → `pnpm install`, `yarn.lock` → `yarn install`, `package-lock.json`/`npm-shrinkwrap.json` → `npm install`); no lockfile ⇒ the install is skipped with a note. The `rift` backend never installs — packages ride the CoW clone. |
| `destroy_command` | no | *(none)* | Teardown run at destroy, inside the checkout, via `$SHELL -lc`, just before the process reaper and the backend remove. `{{path}}`, `{{slug}}` and `{{port}}` substitute. Never blocks the destroy: a non-zero exit or a hang past 120s is logged and the removal proceeds, but it also raises an attention-level warning, because a failed *destroy* teardown never retries — the trigger was the removal that just happened, so whatever it owned is orphaned with no future sweep. |

### `destroy_command` — what it is for

The reaper's `lsof` scans get an 8s budget each (76ms on an idle box, so only a saturated machine reaches it). A blown budget is treated as **unknown**, never as "nothing is listening" — lsof buffers, so a SIGKILLed scan returns zero bytes that parse as a clean empty answer, and reading that as complete skipped the reap and leaked the port block. It retries once and then logs an attention warning naming the worktree.

The destroy-time **process** reaper kills anything holding a listening TCP socket whose cwd is inside the worktree. That covers a hand-started `pnpm preview` or a stray watch runner, and nothing else. Resources a dev server creates *outside* the process tree are invisible to it — **docker containers above all**: a container has no cwd in the worktree, and its published host ports are held by the docker daemon, so neither half of the reaper's match ever fires.

The failure that follows is indirect enough to be hard to read. A stack left running after its worktree is destroyed keeps its host ports; wt frees the slug's dev port and hands it to the next worktree (correctly — it probes, and the *dev* port really is free); that worktree derives the same downstream ports from it and dies on a `port is already allocated` error naming a container wt has never heard of.

`destroy_command` is the hook for releasing that. It is deliberately a shell command rather than anything docker-aware, because the same shape covers tunnels, sandboxes, and per-worktree cloud resources:

```toml
[lifecycle]
# Supabase stamps its project id on every container, network AND volume it
# creates. Match on THAT, not on names — see below. Containers first: a
# network with something attached refuses to go.
destroy_command = """
P=$(printf %.40s {{slug}})
docker ps -aq     --filter label=com.supabase.cli.project=$P | xargs -r docker rm -f
docker network ls -q --filter label=com.supabase.cli.project=$P | xargs -r docker network rm
docker volume ls  -q --filter label=com.supabase.cli.project=$P | xargs -r docker volume rm
"""
```

**Enumerate every resource KIND the tool creates, not just the obvious one.** A teardown that removes containers and stops there leaks networks and volumes silently, and the networks are the ones that bite: docker's predefined address pools are finite, and ~24 orphaned ones exhausted them fleet-wide with `all predefined address pools have been fully subnetted` — an error naming nothing wt- or Supabase-related, so the first guess is always wrong, and nobody can start a stack until it is cleared. Measured on this machine after a containers-only teardown: one stopped project with **0 containers, 1 network and 3 volumes** still present. `docker network prune -f` is the emergency clear; enumerating the kinds is the fix.

**Match on an identity the tool assigns, not on the container name, and test the pattern read-only before you run it destructively.** Two independent ways a name match goes wrong, both silent:

- **Docker's `--filter name=` is an unanchored regex**, so a bare `name={{slug}}` also matches every container whose name merely *contains* the slug — which includes every longer slug that starts with it. Slugs are routinely prefixes of each other, because worktrees derived from the same issue lead with the same id, so destroying `coz-1691` with an unanchored filter tears down the live stack of `coz-1691-domestic-bovid`. Measured: unanchored `coz-1691` matched 12 containers, all of them the *other* worktree's; `_coz-1691$` matched 0.
- **Anchoring is not enough, because the name is truncated.** The Supabase CLI cuts its project id to 40 characters, so a 41-character slug's containers are named for the first 40 — and `--filter name=_{{slug}}$` then matches **nothing at all**. Measured on `meetings-notifies-before-actually-joining` (41 chars): the anchored name filter matched 0 containers while 12 were running. That is the more dangerous of the two, because a teardown that quietly does nothing looks exactly like a teardown with nothing to do.

A **label** filter avoids both: `--filter label=k=v` is an equality test, not a regex, so there is no anchoring question and no prefix collision. `printf %.40s` reproduces the CLI's own truncation, and two slugs sharing their first 40 characters would collide inside Supabase anyway. `com.supabase.cli.project` is the label to use — `com.supabase.cli.workdir` carries the full untruncated path and looks like the better key, but the **database container does not have it**, so a filter on it leaves the heaviest container (and the port block) running.

Whatever you match on, confirm with a read-only `docker ps -a --filter … --format '{{.Names}}'` first, and count the result. The general rule: prefer an identity the tool stamps on what it created over a string you reconstruct, and treat any filter whose semantics are "contains" as unsafe.

Two behaviors worth knowing. It **never fails a destroy** — a broken teardown script leaving a worktree undeletable would be a worse leak than the one this fixes, so a non-zero exit is logged and removal continues, and a command still running after 120s is killed. And a template that mentions `{{port}}` is **skipped entirely** when the slug has no recorded dev port, since that means no dev server ever ran there and the resources it would tear down were never created; templates that don't mention the port always run.

`install_command` caveats: the same command is also used verbatim for the main-clone dependency sync ([backends.md](backends.md)), so it must not rewrite the committed lockfile (use a frozen/`ci` variant) — auto-detection picks frozen variants for that path on its own. The command string is echoed into logs and the activity pane, so don't embed secrets in it. And like `[[actions]]`/`[[automations]]`, it executes automatically — a `.wt.toml` is trusted config, so don't point `wt` at repository config you don't control.

## `[backend]` — optional

Selects how a new worktree is materialized on disk. Omit the whole section for the default. See [backends.md](backends.md) for the full semantics.

| key | required | default | meaning |
|---|---|---|---|
| `kind` | no | `"git-worktree"` | `"git-worktree"` uses `git worktree` (one shared object db). `"rift"` uses copy-on-write clones via the [`rift`](https://github.com/anomalyco/rift) binary — near-instant, `node_modules` copied for free, each checkout an independent clone. |

```toml
[backend]
kind = "rift"
```

The `rift` backend needs the `rift` binary (`npm i -g rift-snapshot`); wt runs `rift init` on the main clone lazily at first create. wt looks for the executable on its own `PATH` first, then asks your login shell (`$SHELL -lc`) — so a wt spawned from a lean environment (launchd, an editor task) still finds a `~/.bun/bin/rift` that only your shell profile adds, and a shell *function* named `rift` can't shadow the real binary. Existing checkouts of the other kind keep working after a flip — the backend that owns a checkout is detected from disk (a `.rift` marker) at removal, never stored. Under `rift`, packages arrive via the CoW clone, so wt skips its own install step and the `--no-install` flag is ignored.

## `[deploy.sst]` — optional integration

Omit the whole section to disable SST awareness entirely — the stage row, `wt stages`, deploy detection, and the per-worktree stage pinning during `wt new` (no `.sst/stage` file is written, and the create output drops its stage line). When present, all three keys are required.

| key | required | default | meaning |
|---|---|---|---|
| `state_bucket` | **yes** | — | S3 bucket holding SST's Pulumi state. |
| `state_prefix` | **yes** | — | Key prefix within the bucket (SST v3 convention: `<state_prefix><stage>.json`). |
| `aws_profile` | **yes** | — | AWS CLI profile with read access to the state bucket. |
| `auto_regen_paths` | no | `["sst-env.d.ts"]` | Files in the main clone that `sst` runs regenerate; restored before fetches so they never show as dirt. |

## `[dev_server]` — optional per-worktree dev server

Omit to disable. When configured, each worktree can run one supervised dev server: started/stopped from the `!` picker (pinned "dev server" group below the always-present "agent" pair `u`/`g` — `d` starts/restarts, `s` stops, `l` opens a live scrollable log overlay; pinned-builtin quick-pick letters are claimed before user `[[actions]]` keys, which fall back to auto-derivation) or `wt dev`, shown in the `dev` row and the bolt badge (the local twin of the SST deploy bolt), opened with `s` (when no SST stage is deployed) and yanked with `y d`.

```toml
[dev_server]
command = "npm run dev -- --port {{port}} --strictPort"
# pnpm: drop the `--` — pnpm forwards it verbatim to the script, so vite
# would see `-- --port …`, read the flags as positionals, and silently
# start on its default port instead of the pinned one:
#command = "pnpm run dev --port {{port}} --strictPort"
```

| key | required | default | meaning |
|---|---|---|---|
| `command` | **yes** | — | Run via `$SHELL -lc` inside the worktree. `{{port}}` substitutes the allocated port (also exported as `$PORT`). Pin the server to it (`--port {{port}} --strictPort` for Vite) — auto-picking servers drift from the recorded port and break hardcoded HMR sockets. |
| `port_base` | no | `8100` | Start of the port range wt allocates from. |
| `port_range` | no | `100` | Range size. Each slug gets a stable port, persisted in wtstate and freed when the worktree is destroyed. |
| `url` | no | `"http://localhost:{{port}}/"` | URL template for the row, `s` open, and yank. |
| `max_concurrent` | no | *(uncapped)* | Most dev servers allowed to run at once across the whole fleet. A start beyond it is refused with exit `75`; `wt dev start --wait` queues instead. See below. |
| `stop_command` | no | *(none)* | Shell command run after the session dies on `wt dev stop` (and when a slot is reclaimed). Same `{{path}}`/`{{slug}}`/`{{port}}` substitution and same never-fatal contract as [`[lifecycle] destroy_command`](#destroy_command--what-it-is-for) — with one exception: `wt dev reset` treats a failure as fatal (see `reset_command`). **Prefer the project's own teardown over raw container commands.** A tool that starts a stack usually keeps state of its own about that stack, and removing its containers behind its back leaves it believing the project is still running — the next start then refuses, with an error that reads as a broken tree rather than a failed stop. See below. |
| `reset_command` | no | *(none)* | Destructive teardown run by `wt dev reset`, between the stop and the start: drop the state `stop_command` deliberately keeps (docker volumes, generated caches). **Skipped entirely when `stop_command` failed** — unlike a destroy, a reset that discards state on top of a still-live environment is worse than not resetting, so wt refuses and says so (exit 1, nothing touched). See below. |
| `health_command` | no | *(none)* | "Is this environment actually usable?" Exit 0 = yes; non-zero = no, first line of stdout is the message. Run on demand only — `wt dev status`, and polled by `wt dev start --wait`. See below. |

Semantics:

- **Survives wt restarts.** The server runs in a tmux session (`<slug>-dev`) on the wt-private server, independent of the TUI process.
- **Crash restart without thrash.** A supervisor loop restarts the command after an exit, backing off 2s, 4s, 8s… to a 60s ceiling; three runs in a row that fail to last **5 minutes** park it as `crashed`. That window used to be 10 seconds, which made the give-up unreachable for exactly the projects where looping costs most: a command that brings up a container stack and then fails on a migration takes far longer than ten seconds to get there, so every attempt read as a long healthy run, the counter reset every pass, and a deterministically-broken twelve-container stack restarted forever — degrading every other worktree's test runs while its row said `starting`. A fast-failing `vite` parked in six seconds; the slow-failing stack never parked at all. While it is retrying, the row and `wt dev status` say **restarting** with the attempt count and last exit rather than `starting`, and `wt dev status` prints the tail of the failure output inline once it parks. The row turns red and the logs stay readable via `wt dev logs` or by attaching to the pane. When the TUI observes that transition, the attention feed and toast show the last useful application-error line plus the `wt dev logs` command, so a startup dependency failure cannot hide behind a red row. A SIGINT/SIGTERM exit counts as an intentional stop, not a crash.
- **Start is restart.** Starting an already-running server kills and relaunches it (picking up config edits).
- **Cleanup is automatic.** The session is killed with the worktree (`wt rm`, `wt clean`) and swept by the startup orphan reaper; the port reservation is freed with the slug's state.
- Vite note: if `vite.config` hardcodes `server.hmr.port`, remove it — HMR then follows `--port` automatically, which is what makes per-worktree instances hot-reload correctly.

### `stop_command` — because stopping a process releases only a process

`pnpm dev` is not one process any more. cozee's runs `supabase start`, which hands twelve containers to the docker daemon and returns; killing the tmux session takes vite down and leaves the stack up. Measured on this machine: four Supabase stacks running, one live dev session. Three of the four were survivors of dev servers already stopped, two of them nineteen hours old.

That matters beyond tidiness, because it is what `max_concurrent` would otherwise be counting. Capping dev *sessions* while the *stacks* leak governs a number with no relationship to the load — the cap would have seen 2 where the machine was carrying 4.

```toml
[dev_server]
# Containers and the network go; VOLUMES STAY. That asymmetry is the point:
# keeping the data is what makes retaking a slot fast, and a network holds no
# state, so leaving it behind is pure leak (see destroy_command above).
stop_command = """
P=$(printf %.40s {{slug}})
docker ps -aq     --filter label=com.supabase.cli.project=$P | xargs -r docker rm -f
docker network ls -q --filter label=com.supabase.cli.project=$P | xargs -r docker network rm
"""
```

Same rules as `destroy_command`, [matching traps included](#destroy_command--what-it-is-for) — do not match on the container name. It runs *after* the session is killed, so the teardown isn't racing a supervisor about to restart what it just tore down. It does **not** run on a restart: `wt dev start` on a running server relaunches the command, and tearing the stack down first would turn every restart into a full cold boot.

`destroy_command` and `stop_command` are separate keys on purpose. A destroy teardown may legitimately be heavier (dropping volumes, deleting generated trees), and running that on an ordinary `wt dev stop` would be a nasty surprise. Setting both to the same line is fine and common.

### `health_command` and `reset_command` — because a port is not readiness

`wt dev start` launches a supervised process and returns. That is the design, and it means **the exit code says "launched", never "ready"**. For a dev command that only runs Vite the distinction is invisible. For one that brings up a container stack, applies migrations and seeds data, it is the whole story: `supabase start` succeeds, the port opens, the URL works — and the migration phase can throw a minute later, in the background, in `wt dev logs`, long after wt reported success. What is left is a serviceable stack on a stale schema behind a green tick.

That cost a full day of misdirected work once. A worktree reported a passing test suite as red (6 files, 20 assertions) because every migration-dependent test was asserting against a half-migrated database; a second worktree was spun up to fix the non-problem; and the comparison run against `origin/staging` was invalid too, because it varied the code while holding the broken database fixed. The failure presents as a defect in the repo, not in the environment, so the investigation goes the wrong way from its first step.

wt cannot ask the question itself — it has no idea what a migration ledger is — so the project answers it:

```toml
[dev_server]
health_command = "scripts/dev/check-migrations.sh"
reset_command  = "docker volume ls -q --filter label=com.supabase.cli.project=$(printf %.40s {{slug}}) | xargs -r docker volume rm"
```

- **`health_command`** runs in the worktree with `$PORT` exported and the usual `{{...}}` substitution. Exit 0 is healthy; anything else is a problem and the first line of stdout (then stderr) becomes the message. It is **on demand only** — `wt dev status`, and polled by `wt dev start --wait`. Never on a poll: a `docker exec psql` against a live stack measured **9 seconds** on this machine, which is fine once and ruinous every fifteen seconds across four worktrees.
- **`reset_command`** is the destructive twin of `stop_command`, run by `wt dev reset` between the stop and the start. `stop_command` keeps the environment's state, which is what makes retaking a slot fast and is right nearly always; this drops it. It exists because the recovery was otherwise folklore — a raw `docker volume rm` nobody could discover from wt, with both obvious in-place repairs actively wrong (`supabase migration up` refuses once newly-arrived stamps sort before the last applied one, and `supabase db reset` wipes buckets that are provisioned at start rather than by migrations).

**Write the health check to distinguish "not yet" from "wrong", or let wt do the waiting.** A check that runs once cannot tell them apart: a migration replay in progress reads 29 of 35 applied, which is indistinguishable from a stale volume stuck at 29, and the one observed settled at 35 about a minute later. `wt dev start --wait` handles this by re-running the check until it passes or the budget expires, so the quiescence wait lives in wt once instead of being reinvented per agent (where it was, and where it was written down wrong). `wt dev status` is a snapshot and says so: an unhealthy answer from a server younger than five minutes is reported as possibly-unfinished startup rather than sending anyone to rebuild.

### The free half: `stale (rebased since start)`

Independent of any of the above, and needing no configuration: wt records HEAD when a dev server starts (`devStartedSha`) and flags the server when that commit is **no longer an ancestor of HEAD** — a rebase, reset or restack rewrote history underneath a running environment. That is exactly when anything the environment derived from the tree is describing a version that no longer exists.

Deliberately not "HEAD moved". Ordinary commits keep the anchor an ancestor and a hot-reloading server absorbs them; flagging those would fire constantly, and a warning that fires constantly is one people learn to scroll past. One `git merge-base --is-ancestor` costs 0.1s, which is why this rides the row's existing poll where the 9-second precise question could not.

### `max_concurrent` — the load governor

A dev server that costs a browser tab's worth of RAM needs no cap. One that costs a twelve-container database stack does: twelve worktrees running one each is 144 containers, and that is a machine you cannot type on.

```toml
[dev_server]
max_concurrent = 4
```

- **A slot is derived, never recorded.** It is held by a live `<slug>-dev` tmux session, read fresh from tmux on every check. There is no counter to release and nothing to drift: a slot frees itself when its session goes, whether that was a stop, a destroy, or a killed tmux server. **A crash is the exception and not an oversight** — see the crashed-holder bullet below, which is the whole reason this list does not say "crashed". The pane dies but `remain-on-exit` keeps the session, so the slot stays held on purpose. If tmux can't be reached, the holders are unknown and the cap does *not* wave everything through.
- **Restart is never blocked.** The asking slug doesn't count against itself, so a full fleet can still pick up a config edit.
- **Parking hands back to wt.** When the supervisor gives up it calls `wt _dev-giveup <slug>`, which saves the pane's scrollback, runs `stop_command`, and ends the session — in that order, because each step destroys the input of the one before. The scrollback is the only copy of *why* it died, and a project guard's refusal (naming the cause and the fix) is exactly the artifact that ends an investigation in seconds; it used to be replaced by "dev server crashed while starting", which says the opposite of what happened. The teardown matters because a parked supervisor is the one case where the supervised process is gone while everything it created outside its own process tree is still up. Ending the session is what frees the slot, through the ordinary derivation rather than any new accounting. The marker deliberately stays `crashed`, so the row still reads crashed with no session and `wt dev logs` serves the saved scrollback.
- **A repeated fast failure stops early.** Two consecutive failures under 10s agreeing on both the exit code *and* the child's last output line are treated as deterministic, and the supervisor parks without spending its remaining attempts. Both signals are required: everything fails with exit 1, so the code alone would park genuine flakes on their second try. If the last line cannot be read the signature is **unknown**, never empty — an empty string would compare equal to the next unknown one and silently collapse the test back to exit-code-only.
- **A crashed server still holds its slot** until it parks and hands back (above), or until a reclaim sweep takes it. The supervisor parked, but the containers its command created outside its own process tree are still up, and those are what is being rationed. The refusal message names crashed holders specifically, since they are the cheapest slot to reclaim.
- **Reap-on-acquire.** When a start finds the fleet full, dev sessions belonging to no live worktree are killed (running their `stop_command`) and the count retaken before refusing. Scope is deliberately narrow: a dev server whose worktree still exists is somebody's, even with no agent attached.
- **It is a governor, not a mutex.** Two starts racing at the same instant can both see the last slot. That overshoot is one extra dev server; a real lock would have to be *released*, and a lock that must be released is the drifting counter this design exists to avoid.

Refusal is exit `75` (sysexits `EX_TEMPFAIL`), distinct from `1` so a looping agent can tell "try later" from "the dev server is broken". `wt dev start --wait` queues instead of refusing, and `wt dev queue <slug> --first` moves one waiter ahead of the rest when somebody with fleet context says it goes first — see [cli.md](cli.md#wt-dev-startstopstatuslogs).

## `[issue_tracker]` — optional integration

`read_command = ["tracker", "task", "read", "{id}"]` configures a headless
full-task reader for `wt issue --read` and the start skill. It is an argv array,
not a shell string: `{id}` is substituted literally in arguments, the executable
is fixed, and the command runs in the destination worktree. Omitting it preserves
an inherited reader, or leaves reading disabled when none is inherited. Set `[]`
to disable reading explicitly, including an inherited reader. The reader owns authentication,
Markdown rendering, attachment downloads, and any local artifacts. wt preserves
stdout/stderr and nonzero exit status (except reserved exit 3, mapped to 1);
a 300-second timeout exits 124 and reports
incomplete context. Ordinary issue-link lookup never starts the reader.

Omit the section entirely to hide the `issue` row. The section's mere presence surfaces the issue id parsed from the branch slug (`yourname/eng-1883-fix` → `ENG-1883`) as an unlinked value — useful when your tracker has no per-task URLs. Add `url_template` (or the Linear preset) to turn the id into a deep link, which also powers the `i` open-issue key and the `y i` yank.

```toml
# Bare id display only — no per-task URLs exist:
[issue_tracker]

# Generic tracker with task URLs:
[issue_tracker]
url_template = "https://tracker.example.com/browse/{id}"

# Linear preset (derives url_template = "linear://<workspace>/issue/{id}"):
[issue_tracker.linear]
workspace = "acme"
```

| key | required | default | meaning |
|---|---|---|---|
| `url_template` | no | *(unset)* | URL with an `{id}` placeholder, substituted with the uppercased issue id parsed from the slug (no API calls, no token). Wins over the Linear preset when both are set. |
| `prefix` | no | *(unset)* | Required id prefix (lowercase, e.g. `"coz"`) for **new** worktree branches. When set, `wt new GH-970 …` (or any other prefix) fails with guidance instead of minting the branch — a GitHub issue attaches as the *secondary* id (`wt new … --gh <n>` / `wt issue <slug> --gh <n>`), never as the worktree's identity. Attaching to existing branches is exempt. |
| `linear.workspace` | `[issue_tracker.linear]` present: **yes** | — | Linear preset: derives `url_template = "linear://<workspace>/issue/{id}"` (the desktop-app deep-link scheme). |

Id parsing itself is driven by the slug shape (`[a-z]+-\d+`), independent of `[branch] id_pattern`.

**Setting an id by hand:** a slug that carries no id (or the wrong one) takes an override — `wt issue <slug> --id COZ-2185`, or `#` in the TUI. Three states, not two: `--clear-id` DROPS the override (back to the slug's own id), while `--no-id` — or an empty `#` prompt — asserts the worktree has **no** tracker issue. On a slug that carries an id those are different answers, and only the second can detach it. Every reader resolves *override first, slug second*, so this is what the issue row links, what `{{issue_id}}` renders, and what `requires = ["issue.tracker"]` tests. It is deliberately an override rather than a cached parse: nothing backfills slug-derivable ids into state, because a derived value in storage goes stale the moment a branch is renamed while still looking authoritative.

**Built-in `GH-` convention:** an id with the `gh` prefix (`yourname/gh-970-fix-typo` → `GH-970`) is taken to mean a GitHub issue on this repo and links to `<origin repo>/issues/970`, bypassing `url_template`. The repo URL is derived from the main clone's `origin` remote (ssh, scp, or https forms; bare ssh-config aliases can't be resolved, so those ids render unlinked). No configuration — this works even with a bare `[issue_tracker]` section. (With `prefix` set, GH-led ids can't *create* worktrees — the supported shape there is the secondary id below.)

**Secondary GitHub issue:** independent of the slug id, a worktree can carry an attached GitHub issue number (`wt new … --gh 970`, `wt issue <slug> --gh 970` — see [cli.md](cli.md)). The issue row shows it after the primary (`ENG-1935 · #970`), and it becomes the most-specific target for `i` / `y i`; `I` / `y I` keep targeting the primary.

## `[harness]` — coding-agent default

The initial primary harness used by F12, agent messages, prompt actions, and
`harness = "primary"` integrations. An intentional selection made with Tab
is stored per repository and wins until changed; repositories with no saved
selection use this configured default.

```toml
[harness]
primary = "codex"
hidden = ["opencode"]
```

| key | required | default | meaning |
|---|---|---|---|
| `primary` | no | `"claude"` | `"claude"`, `"codex"`, or `"opencode"`. Used when this repository has no persisted override. |
| `hidden` | no | `[]` | Harnesses omitted from Tab cycling, automatic live-session routing, TUI session discovery, picker entries, and activity polling. Messaging has no harness override, so a hidden harness is not selected for a new send. The configured `primary` cannot be hidden, and at least one harness must remain visible. |

## `[naming]` — optional generated worktree names

Omit to disable generated title/brief/description text. Naming invokes the
configured coding-agent harness's non-interactive CLI using its existing
authentication; wt does not require or call a separate model API. Runs are
serialized, short-lived, and read-only. Codex runs ephemerally without project
rules, Claude disables tools and session persistence, and OpenCode uses its
isolated `--pure` mode.

```toml
[naming]
harness          = "primary"
reasoning_effort = "low"

[naming.models]
codex = "gpt-5.6-luna"
```

| key | required | default | meaning |
|---|---|---|---|
| `harness` | no | `"primary"` | `"primary"` follows the repository's effective primary harness; `"claude"`, `"codex"`, or `"opencode"` pins naming independently. |
| `models.<harness>` | no | *(harness default)* | Harness-native model override for `claude`, `codex`, or `opencode`. Per-harness keys keep `harness = "primary"` valid when the selected primary changes. |
| `reasoning_effort` | no | `"low"` | Naming-only effort/variant: `minimal`, `low`, `medium`, `high`, `xhigh`, or `max`. Claude maps `minimal` to `low`. Model support is enforced by the selected CLI. |
| `max_input_tokens` | no | `8000` | Soft prompt budget; diff hunks are dropped largest-first to stay under it. |
| `timeout_ms` | no | `120000` | Per-process timeout, including harness startup. |

Summaries are content-addressed by a hash of the diff, so identical diffs (across rebases, amends, branch renames) reuse the cached result.

## `[browser]` — legacy settings

wt opens links directly through macOS (`open <url>`), letting the default
browser choose its active profile. Custom schemes such as `linear://` use
their registered application. `browser.chrome_profile` is still accepted for
compatibility with existing configs, but is ignored and can be removed.

## `[github]`

| key | required | default | meaning |
|---|---|---|---|
| `reviewers` | no | `true` | Human-reviewer workflow. Set `false` in a repository `.wt.toml` when the repo does not use human code review: hides the human-review badge and PR metadata, removes the review-requests section, disables the `v` reviewer picker and the reviewer leg of `E`, and suppresses `review.changes_requested` automations. The separate `[review_bot]` track is unaffected. |
| `ignored_review_repositories` | no | `[]` | Exact GitHub `"owner/repository"` names to omit from the pinned review-request section. Matching is case-insensitive and applies to every open PR in the named repositories. |
| `ignored_checks` | no | `[]` | Glob patterns (case-insensitive, `*` wildcard only) matched against check names; matching contexts are dropped from the PR checks rollup so non-CI checks don't flip the badge. The configured `[review_bot]`'s `check_contexts` are always excluded automatically — no need to repeat them here. |
| `default_reviewer` | no | *(unset)* | GitHub login requested by the `E` ("ship it") chord (mark ready + request reviewer + arm auto-merge). Unset disables the reviewer leg. |
| `pr_target` | no | `"github"` | Where `p` opens PRs: `"github"` keeps GitHub URLs, `"linear"` rewrites them to Linear Reviews deep-links. `g p` / `l p` always open GitHub / Linear explicitly. |

## `[review_bot]` — the bot-review track

The badge/row/automation track for an automated PR reviewer. Omit the whole section for the default **CodeRabbit** preset — the exact behavior wt always had. Configure it to point the track at any other bot; the bot's own check contexts are always excluded from the CI rollup (an advisory reviewer must never flip the checks badge — it has its own badge).

Two "unresolved" models, selected by `unresolved_via`:

- `"threads"` (default, CodeRabbit-shaped): unresolved = review threads opened by `login` that aren't resolved.
- `"checklist"`: the bot posts a summary comment carrying a `- [ ]` task list; unresolved = unticked boxes (ticking a box in the GitHub UI is how items are accepted/dismissed). Use this when the bot posts via `github-actions[bot]` — that login is shared by every workflow, so the comment shape (`summary_marker`) is the discriminator. A bot may keep **several** live checklists at once, which is why `summary_marker` takes a list: latest-wins applies within each marker, and the counts sum across them.

```toml
# Example: an in-repo GitHub Actions "Codex review" workflow
[review_bot]
name           = "Codex"
login          = "github-actions"   # "[bot]" suffix optional
check_contexts = ["Codex code review", "Prepare Codex review", "Announce Codex review"]
unresolved_via = "checklist"
# The full pass and the per-commit delta log are independent checklists.
summary_marker = ["### 🤖 Codex review", "### 🤖 Codex follow-up reviews"]
pending_marker = "🤖 ⏳ Codex review started"
rerun_command  = "/codex-review"
```

Note what is deliberately absent from `check_contexts`: the workflow's `Codex review complete` job, because it is a **required** check in the base branch's ruleset. Listing it would exclude a real merge gate from the checks badge, which is the opposite of the exclusion's purpose — the rule is "an advisory run must not flip the CI badge", not "anything the bot owns is invisible".

| key | required | default | meaning |
|---|---|---|---|
| `name` | no | `"CodeRabbit"` | Display name (built-in re-run action label). |
| `login` | checklist: **yes** | `"coderabbitai"` | The bot's comment/thread author login. A trailing `[bot]` is ignored when matching (GraphQL reports app logins without it). Required in checklist mode — the CodeRabbit default would silently match nothing. |
| `check_contexts` | no | threads: `["CodeRabbit"]`, checklist: `[]` | Glob patterns for the bot's check contexts / workflow job names. Drive pending-vs-done detection and are auto-excluded from the CI rollup — set them whenever the bot runs as checks/jobs so an advisory run can't flip the CI badge. |
| `unresolved_via` | no | `"threads"` | `"threads"` or `"checklist"` (see above). |
| `summary_marker` | checklist: **yes** | — | String, **or a list of strings**, identifying the bot's summary comment(s). Matched at the start of any of the comment's **first three lines** (see below). Each marker tracks its own latest comment and the unticked counts sum, so a bot posting a full pass and a rolling delta log under different headings has both counted. A comment matching more than one marker is filed under the longest. |
| `pending_marker` | no | *(unset)* | Same matching, for the bot's "review started" ack comment. An ack newer than the latest summary shows as *pending* — needed for comment-triggered re-runs, whose check runs never attach to the PR head. |

**Marker matching.** Both markers match at the start of any of the comment's first three lines, not strictly at the start of the body. The reason is that the conventional way to make a comment machine-identifiable is an HTML comment on its own first line, with the visible heading below it:

```markdown
<!-- codex-review-summary -->
### 🤖 Codex review
```

Under a strict prefix test, `summary_marker = "### 🤖 Codex review"` matches nothing there and the badge sits blank with no error to chase — two repos running variants of one reviewer workflow differed on exactly this. The three-line window is deliberate rather than a substring search: a human (or the bot) quoting the heading further down a long comment must not promote that comment to "the summary". Either line works as the marker; prefer the HTML comment when the workflow emits one, since it's the half that exists to be matched.
| `rerun_command` | no | *(unset)* | PR comment body that re-triggers a review. When set, a built-in "Re-run *name* review" action appears in the `!` picker (it posts the comment via `gh`). |

**Staleness.** A review can lag the branch head, and wt marks that state **stale**: the details row shows `(old head)` and a *clean* badge renders in the warning colour rather than green.

That colour rule is a reversal, and the reason it flipped is worth keeping. It used to leave stale-clean green, on the argument that a bot which only reviews on `opened` is stale as its *steady state*, so dimming it would make clean-green a colour you'd never see. Bots that post per-commit delta reviews broke that premise: stale became a transient again, and green-with-prose is the shape the repo rules call a false green — a contradicting fact sitting in prose beside a badge does not exist, and `(old head)` was that prose. A false yellow costs a look; a false green costs the review.

Staleness is answered three ways, in descending order of directness:

1. **The sha the bot stamps.** A per-commit reviewer names the commit it reviewed, and that is an assertion rather than an inference. wt looks for the head's 7-character prefix anywhere in a live checklist body, which covers both shapes Codex emits (a `#### \`28daaa2\`` section heading and a `<!-- codex-review-state:v1 {…,"head":"28daaa28…"} -->` trailer) without wt knowing either format. Positive-only: a bot that never mentions shas falls through to the rest unchanged.
2. **The bot's `check_contexts` on the head commit.** The checks rollup is read off the head, so a bot context there means its pipeline ran there.
3. **The timestamp proxy** — the newest summary predating the head's commit date.

The order matters because (2) lags (1) by minutes: the workflow attaches its check runs well after it posts the comment, and in that gap a review that has already come back clean reads as stale. That window is what put a yellow badge on a PR whose delta review said *No material issues found*. And (3) alone is not enough for a bot whose delta log is one comment appended to per commit: its `createdAt` is the first delta's, so it reads stale forever after.

Badge states: pending (running / re-run acked), unresolved (with count), clean, none — unresolved wins over a concurrent re-run, since old findings still need addressing. Checkbox counting skips fenced code blocks, so a suggestion block quoting checkbox syntax doesn't inflate the count. One sizing note: the summary comment is found within the PR's most recent 30 comments — on an extremely chatty PR whose last 30 comments postdate the bot's summary, the badge reads as if the bot never ran.

**Drafts.** In `threads` mode the badge hides on draft PRs: clean is inferred from the bot's check context completing, and CodeRabbit's "review skipped — draft detected" run completes exactly like a real review, so a skipped draft would read green. `checklist` mode has no such hazard (clean requires a summary comment the bot actually posted, so a skip yields `none`), and those bots typically review drafts, so the badge shows there.

The badge keeps the carrot glyph for CodeRabbit and switches to a checklist glyph for any other login (deliberately not a robot — that's the Claude harness session glyph).

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

## `[editor]`

Which editor `wt open`, the TUI's `o` / `O`, the slot palettes' `z` row, and `wt new --open` launch.

| key | required | default | meaning |
|---|---|---|---|
| `command` | no | *(unset — the built-in Zed integration)* | Shell command run via `$SHELL -lc`. `{{path}}` substitutes the checkout path, shell-quoted; a command that never mentions it gets the path appended (also quoted), so a bare `cursor` works. |

```toml
[editor]
command = "cursor {{path}}"      # or: "code -n", "idea", "zed -n {{path}}", "open -a Emacs {{path}}"
```

Leaving the section out selects **Zed**, with focus-if-already-open tracked through yabai. Setting `command` replaces that path; focus-if-open becomes the editor's responsibility. Both paths leave the terminal visible unless opted in through `ui.hide_terminal_apps`.

## `[ui]`

Terminal hiding is opt-in on macOS. For example:

```toml
[ui]
hide_terminal_apps = ["ghostty"]
```

`hide_terminal_apps` defaults to `[]` (disabled). Entries are macOS application
process names, matched case-insensitively, such as `ghostty`, `alacritty`, or
`wezterm-gui`. Only a matching frontmost app is hidden, before opening a browser
link or editor. No background terminal is hidden. This uses System Events
Automation permission and is best-effort if permission is unavailable.

`action_groups_last` defaults to `[]`. Set it to `["dev server"]` to move that
group below the other groups in the `!` action menu. Listed groups retain their
item order and shortcut keys; unknown names are ignored. The custom-prompt
entry remains last.

| key | required | default | meaning |
|---|---|---|---|
| `rows` | no | `["branch", "issue", "stage", "dev", "pr", "claude", "git"]` | Detail-pane row order. Available ids: `branch` (renders `<branch> → <base>`; the branch gives up cells first when the line is tight, so the target is always readable), `path`, `issue` (legacy alias: `linear`), `stage`, `dev`, `pr`, `claude`, `git`. Unknown ids are ignored (including the retired `base` id, now merged into `branch`, and the retired `status` id — the asserted work status now renders as a fixed full-width banner at the top of the pane, so its note never truncates); omitted ones are hidden. A row also hides itself when its integration isn't configured (e.g. `issue` without `[issue_tracker]`, `dev` without `[dev_server]`). The rebase state (restacking / mid-rebase / conflict + files) isn't a row — it renders as a fixed block below the rows, above the AI summary. |
| `hidden_badges` | no | `[]` | Glyph slots to suppress from the **list-pane** badge cluster. Ids: `action` (running action), `dirty` (uncommitted-changes pencil), `rebase` (restacking / conflict), `deploy` (SST-or-dev-server bolt), `session` (harness glyph), `review_bot`, `review` (human review), `pr` (PR state, doubling as the merge-queue slot), `checks` (CI rollup). Unknown ids fail the load. Opt-out rather than an ordered allow-list like `rows`, because the cluster's left-to-right order is designed (`[bot] [review] [pr] [checks]` reads as one "state of this PR" run) and an allow-list would hide slots added in later versions. Details-pane segments are unaffected — hiding a badge declutters the list without losing the signal. |
| `activity_pane` | no | `"column"` | Where the activity pane (harness sessions, action runs, the attention and firehose feeds) sits. `"column"` puts it in the right-hand column under the details pane, so the list pane gets the full TUI height — the better default when the board is long enough to scroll. `"full_width"` spans it across the bottom under both panes, capping list+details at 20 rows, which is what the layout was before the column split. The trade is the one you'd expect: `full_width` buys the feed the whole terminal width before its word-wrap kicks in (attention lines are mostly word-wrapped status notes, so width is what they spend), and pays for it in list rows. Neither changes what renders, only the box it renders in. |
| `sort` | no | `"status"` | Row ordering inside each list section. `"status"` ranks rows by work-status urgency — `ready` (the merge is yours to do), `needs-human`, `needs-testing`, `review`, `working`, statusless, `todo`, then merged/gone last — with the manual order as the stable tie-break, so same-status rows keep their hand order and `J`/`K` still reorders within a rank (a cross-rank nudge is refused with a hint). With no statuses asserted it's a pure no-op. `"manual"` restores the pure hand order. Stack sections always keep spine order either way. The cursor tracks the worktree, not the position, so a re-sort never moves your selection. |

## `[skills]`

| key | required | default | meaning |
|---|---|---|---|
| `startup_check` | no | `true` | Check wt's bundled agent skills + managed instructions block for pending updates when the TUI starts, prompting y/n once per update before the terminal is taken over (so agent sessions spawned from that run see the updates). A "no" is remembered per content version and never re-asked; copies wt didn't install are never overwritten without an explicit yes. `false` disables the startup prompt — `wt skills` keeps working on demand. See [skills.md](skills.md). |

## `[manager]`

| key | required | default | meaning |
|---|---|---|---|
| `wt_feedback` | no | `false` | Standing brief for the [manager session](manager.md): proactively send workflow papercuts/nits observed during fleet work to the session working on the wt source repo, which reviews and applies them. Opt-in because it presumes you run such a session; the manager skill reads this flag from the config TOML at session time. |

## `[update]`

| key | required | default | meaning |
|---|---|---|---|
| `startup_check` | no | `true` | Check the wt source clone for upstream commits when the TUI starts — at most once a day — and prompt y/n to fast-forward ([`wt update`](cli.md#wt-update-log---check---head) semantics: CI-green target selection, post-pull boot probe with auto-revert, skipped silently when the clone is dirty or ahead, a "no" remembered until the offer changes, re-exec on accept — see [updates.md](updates.md)). `false` disables the startup check — `wt update` keeps working on demand; `WT_UPDATE=off` disables the whole update system (check, boot sentinel, rollback offers) for a single run. |

## `[[actions]]` — the `!` menu

Pre-built actions surfaced by the `!` picker (and available as automation targets). Two kinds, distinguished by which field you set:

- **Prompt actions** (`prompt = "…"`): run the worktree's primary coding agent. Default delivery is a tracked headless run (`claude -p` / `codex exec` / `opencode run`); `target = "session"` instead sends the prompt to the live F12 session, and `target = "manager"` sends it to the singleton [manager session](manager.md) prefixed `[re: <slug>]` (both fire-and-forget: no completion signal, so `affects` won't auto-refresh). Claude prompts are submitted at the live session's own prompt (see [manager.md](manager.md#how-a-message-reaches-a-session)); other harnesses retain their pane adapters. Manager-target actions appear in **both** pickers: row-scoped in `!`, and again in the `M` [manager palette](manager.md#the-command-palette-m), where they launch against the row selected when the palette opened.
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
| `target` | prompt only | `"headless"` | `"headless"`, `"session"`, or `"manager"` (see above). |
| `affects` | both | prompt: `["git", "github"]`, shell: `[]` | State domains the action mutates; the matching caches are refreshed when the run exits. Tags: `git`, `github`, `dev` (the worktree's `[dev_server]` state). Explicit `[]` opts out. |
| `requires` | both | `[]` | Preconditions; unmet entries gray out in the picker with the reason. Tags: `pr` (any PR exists), `pr.ready` (open non-draft PR), `deployed` (this worktree's SST stage is live), `issue.tracker` (the worktree resolves to a tracker id, i.e. `{{issue_id}}` renders non-empty — settable with `#` / `wt issue --id` when the slug carries none). |
| `key` | both | auto-derived | Single-char quick-pick letter in the `!` menu. Lowercase only — keys are case-folded and the picker matches `a-z`, so an uppercase key silently degrades to auto-derivation. With `[dev_server]` configured, `d` and `s` are claimed by the pinned built-ins. |
| `group` | both | ungrouped | Section label; same-group actions cluster under one header. |
| `arg_prompt` | both | *(unset)* | Label for a per-launch value prompt. Picking the action first shows recent values (from `~/.cache/wt/action-history.json`) plus a "new…" input; the value substitutes `{{arg}}`. |
| `label_extract` | both | *(unset)* | Regex (source string, no flags) scanned against the run's output; the last per-line match (capture group 1, or the full match) becomes the history label for the `{{arg}}` value. |
| `external` | both | `false` | This action's effect **leaves the repository** — it moves a ticket, posts a message, calls someone else's API. Terminal success *and* failure then narrate on the **attention feed** instead of the firehose. Same rule [`builtin:close-issue`](automations.md) follows and for the same reason: wt's undo does not reach outside and the board shows nothing, so a success nobody saw is a change nobody can find, and a failure nobody saw is a change everybody assumes happened. Deliberately not implied by `affects`, which says which of *wt's own* caches to invalidate — a statement about the inside. A kill stays on the firehose either way: you pressed the key, and nothing ran. |

**Template variables** (`{{var}}`, unknown vars pass through so typos are visible): `{{base}}` resolved diff base, `{{base_branch}}` parent branch or trunk, `{{branch}}`, `{{slug}}`, `{{cwd}}` worktree path, `{{pr}}` PR number or empty, `{{issue_id}}` the worktree's tracker id — the `wt issue --id` / `#` override when set, else the id carried in the slug (`coz-2176-active-louse` → `COZ-2176`); empty when neither exists, empty rather than the slug, because an obviously-wrong request beats a plausible one against the wrong issue, `{{stage}}` the worktree's SST stage, `{{skill_prefix}}` the harness's skill-invocation prefix (`/` for Claude Code, `$` for Codex/OpenCode — write `{{skill_prefix}}restack` to invoke a skill portably), and `{{arg}}` for the collected `arg_prompt` value. One var needs no subject worktree and so works in the row-less manager/slot palettes too: `{{today}}` renders the current weekday and date at dispatch time (`Tuesday, August 11, 2026`) — a long-lived session's weakest fact, since the model has a training cutoff and a compaction summary carries no timestamp. The built-in compact actions use it; an explicit var of the same name overrides it.

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
| `on` | **yes** | — | Trigger: `pr.checks.failed`, `review_bot.unresolved` (the `[review_bot]`'s findings; `rabbit.unresolved` is a legacy alias), `review.changes_requested`, `pr.conflict`, `wt.merged` (a non-stacked worktree landed), `stack.parent_merged` (a stack member's parent landed), `status.needs_human` / `status.needs_testing` / `status.ready` (the worktree's asserted work status is that state; hyphenated spellings are accepted aliases). |
| `run` | **yes** | — | An `[[actions]]` id, or a builtin: `builtin:restack` (only valid with `stack.parent_merged`), `builtin:clean` (any single-worktree trigger), `builtin:notify` (any trigger; a macOS banner), `builtin:close-issue` (only valid with `wt.merged`; closes the worktree's attached GitHub issue as completed once the branch lands), `builtin:delete-branch` (only valid with `wt.merged`; deletes the branch's ref on the origin repo once it lands, the same effect as GitHub's "Automatically delete head branches" setting for repos that have not enabled it). |
| `busy` | no | `"queue"` | When the worktree isn't quiescent at delivery time: `queue` holds the intent until it settles, `skip` drops it. |
| `cooldown_minutes` | no | *(none)* | Minimum minutes between dispatches per (rule, worktree). |
| `branch` | no | *(none)* | **`branch.advanced` only** (required there, rejected elsewhere): which branch to watch. Must be `[branch] base` or listed in `[branch] keep_fresh` — nothing else advances a local head, so watching an unfetched branch is a rule that can never fire. No default on purpose: guessing the trunk would watch the wrong thing on exactly the fleets this exists for, where the release branch is the one you *don't* fork from. |
| `after_days` | no | `2` | **`status.verification_overdue` only** (rejected on any other trigger): how long an outstanding post-merge verification may sit before the rule starts firing. A knob because deploy cadence is the one thing wt cannot guess — a fleet that deploys on merge wants `1`, one that batches a weekly release wants `7`, and a default wrong in either direction turns the reminder into noise or into nothing. |
| `settle_seconds` | no | `120` (merge triggers: `10`; status triggers: `0`) | Quiescence window: the condition must hold and the worktree be edit-free this long before delivery. Doubles as your cancellation grace period. Status triggers default to 0 — an assertion is a deliberate write, not flappy derived state. |

At runtime, `A` pauses all automations and `Ctrl+A` pauses the selected worktree (or its whole stack); both persist across restarts.
