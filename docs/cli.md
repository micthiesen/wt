# CLI reference

`wt` with no arguments launches the TUI (when stdout is a TTY; piped output falls back to `wt ls`). Everything below is the one-shot subcommand surface. `wt <cmd> --help` prints per-command usage.

Environment variables: `WT_CONFIG` points at an explicit config file; `XDG_CONFIG_HOME` relocates the default lookup (see [configuration.md](configuration.md)). Both are forwarded into the `wt events` launchd daemon so it loads the same config.

## Worktree lifecycle

### `wt ls`

List all non-main worktrees (slug, stage, PR, status).

- `--json` — machine-readable array (slug, branch, path, stage, status, dirty, linear_url, …).

### `wt new <linear-url|id|branch|slug>`

Create a worktree from a Linear URL/ID, an existing branch name, or a bare slug. Runs the full setup: fetch, `git worktree add`, env-file copy, SST stage pin, package install.

- `--slug <s>` — explicit slug when creating from a Linear id.
- `--base <ref>` — fork base to branch from (recorded; see `wt base`).
- `--any` — match branches by any author, not just your `branch.prefix`.
- `--open` / `--no-open` — open in Zed after creation (default: open when interactive).
- `--no-install` — skip the package-install step.

If the branch already has a worktree, prints its path instead of erroring.

### `wt rm [<slug>]`

Remove a worktree (with dirty/unpushed guards, optional SST stage destroy, optional branch delete). No slug ⇒ interactive picker.

- `--yes` / `-y` — skip confirmations.
- `--force` — remove despite uncommitted / unpushed work.
- `--destroy-stage` / `--no-destroy-stage` — force the SST stage decision (default: prompt when your stage looks deployed).
- `--delete-branch` / `--keep-branch` — default deletes the branch.
- `--background` / `-b` — dispatch as a background job (watch with `wt logs <slug>`).

### `wt clean`

Remove every worktree that is merged or whose remote branch is gone. "Gone" is only auto-cleaned when a merged PR confirms the content actually landed; anything riskier is left for an explicit `wt rm`.

- `--yes` / `-y` — skip confirmation (required non-interactively).
- `--destroy-stage` / `--no-destroy-stage` — apply to all candidates (default: per-worktree, destroy iff its stage is live).
- `--foreground` — run removals synchronously (background dispatch is the default here, unlike `rm`).

### `wt doctor [<slug>]`

Health report: working tree, sync vs trunk, SST stage pin + deploy state, node_modules, locks, merged status, PR/CI. One worktree (or the one containing cwd), or all.

- `--all` / `-a` — force the full summary table.
- `--json` — machine-readable.

### `wt open [<slug-or-query>]`

Open a worktree in Zed. Exact slug or case-insensitive substring; no query ⇒ interactive picker.

## Inspection & maintenance

### `wt stages`

List SST stages in the configured state bucket and flag orphans (no matching live worktree). Requires `[deploy.sst]`.

- `--clean` — destroy orphaned stages (`sst remove` per stage, in the main clone).
- `--yes` / `-y` — skip the destroy confirmation.
- `--json` — machine-readable `{live, orphaned}`.

### `wt logs [<slug>]`

Tail a destroy log (`tail -F`). No slug ⇒ the most recently modified log.

### `wt base <slug>` / `wt base set <slug> <ref>` / `wt base clear <slug>`

Show / record / forget a worktree's fork base — the branch it forked from when that isn't trunk. The TUI's base row, sync counts, diff, and AI summary all resolve against it. A stack manifest, when one exists, wins over this record.

### `wt size [<path>...]`

Production-LOC + file count for a diff (excludes tests, snapshots, generated files, lockfiles) — the budget you split stacks against.

- `--base <ref>` — diff base (default `origin/<branch.base>`).
- `--stack <stackId>` — per-slice breakdown for a stack (auto-selected when run on a holistic branch with a manifest).
- `--json` — machine-readable.

## Stacked PRs

### `wt stack <sub>`

The stacked-PR manifest engine — see [stacking.md](stacking.md) for the workflow it powers.

| sub | what it does |
|---|---|
| `plan --from <file>` | strict-validate + ingest a manifest JSON (no side effects); prints the `stackId` |
| `apply <stackId>` (or `--from <file>`) | materialize the manifest: a worktree + draft PR per slice. `--install` runs installs per slice (default off); `--verify` typechecks each cumulative prefix first (needs `[stack].verify_command`) |
| `status [stackId]` | render the stack DAG + drift vs live PR state. `--json`, `--all` |
| `rebase [stackId]` | `reconcile` + `replay` in one shot — the post-merge restack. `--onto <ref>` |
| `reconcile [stackId]` | bookkeeping only: mark merged slices, reparent children (incl. a landed stack-on-stack parent) |
| `replay [stackId]` | squash-safe replay of each slice onto its moved parent + PR base retarget |
| `add [<branch>] [<stackId>]` | append an existing branch to a live stack as a new tip slice (adopts its open PR or opens a draft). `--onto <sliceId\|branch>`, `--title <t>` |
| `split <stackId> <sliceId> --from <frag>` | reshape: replace one slice with N sub-slices, re-threading descendants. `--plan` previews; `--apply` chains apply + replay |
| `hunks <file>...` | list a file's holistic-diff hunk ids for hunk-level partitioning (consumed by `/split`). `--holistic <branch>`, `-U <n>`, `--json` |
| `section <stackId> <sliceIdOrPr> [label]` | print a slice's static PR-body "Stack" section |
| `context` | read-only pre-`/split` dump: base decision, changed files, `wt size` |
| `prune-backups` | delete `backup/restack-*` branches older than `--days <n>` (default all) |

On a merge conflict, `replay`/`rebase` exit 3 and name the failing branch + backup branch — `wt` never auto-resolves conflicts; the `/restack` skill (or you) does.

### `wt skills install [<name>...]`

Install wt's bundled agent skills (`split`, `restack`, a `wt` reference skill). No names ⇒ all.

- `--harness <claude|codex|opencode>` — copy into that harness's native skills dir.
- `--rulesync` — copy into a rulesync source dir instead (`--dest` overrides, `--build` regenerates immediately). Mutually exclusive with `--harness`.

## Integrations

### `wt events <sub>`

The optional GitHub webhook daemon — see [github-events.md](github-events.md).

| sub | what it does |
|---|---|
| `install` | write the launchd agent + generate the HMAC secret; prints the values to paste into GitHub's webhook settings |
| `start` / `stop` | load / unload the launchd agent |
| `status` | liveness, bind address, pid, delivery count, last fetch/error, snapshot age |
| `secret` | generate or show the HMAC secret |
| `uninstall` | unload + remove the launchd agent |
| `serve` | run the daemon in the foreground (what launchd invokes) |

### `wt claude <sub>`

Drive a worktree's Claude Code tmux session from scripts or other sessions.

| sub | what it does |
|---|---|
| `send <slug> [text...]` | upsert the worktree's primary Claude session (cold-starts it if absent) and paste + submit the text; reads stdin when no text args (heredoc-friendly). Accepts a branch name in place of the slug. Fire-and-forget |
| `ls` | list slugs with a live Claude session |
| `kill <slug>` | kill the worktree's primary Claude session |

---

There is also an internal `wt _destroy` entrypoint that `rm --background` / `clean` spawn for background removals — not for direct use.
