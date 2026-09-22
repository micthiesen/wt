---
name: wt
description: >-
  Use the `wt` CLI/TUI to manage git worktrees: create, list, inspect, remove,
  and drive per-worktree coding-agent sessions, plus stacked PRs (worktrees
  based on other worktrees, restacked with `wt restack`) and agent-asserted
  work statuses (`wt status`). TRIGGER when the user mentions wt, worktrees,
  "a wt", stacking, or restacking. For conflict resolution during a restack
  use the dedicated /restack skill; this is general orientation.
targets:
  - '*'
---
# wt — worktree CLI/TUI

`wt` is a terminal UI + CLI for keeping multiple git worktrees in flight at
once. A main clone stays on the trunk branch (`[branch] base`); active
branches live in per-worktree checkouts under a configured `worktree_root`.
Each row shows live git status, PR state, preview-deploy state, issue
id/link, review-bot state, work status, and coding-agent session activity.

Config lives at `~/.config/wt/config.toml`, with the nearest `.wt.toml` (from
cwd upward) recursively merged over it for repo-specific settings like the
trunk branch (`wt` refuses to start without the required fields; the loader
reports every missing field at once). The standard install aliases
`wt='~/.wt/bin/wt'`; if `wt` isn't found in a non-interactive shell, invoke
`~/.wt/bin/wt` directly.

## Under `[backend] kind = "rift"`, worktrees are clones

Rift worktrees are copy-on-write **clones**, not git worktrees: each has its
own `.git` and its own remote-tracking refs, so `git worktree list` in the
main clone shows only itself. Those refs go stale per-clone. **Run
`git fetch` inside a worktree before any ahead/behind, merge-base, or
`origin/<base>..` comparison** — otherwise the numbers describe whatever
`origin/<base>` pointed at when the clone was cut, and `wt doctor`'s sync
column reads the same stale refs. Surveying several worktrees means fetching
in each one.

## Subcommands

- `wt` — interactive TUI (vim keys: `j`/`k`/Enter, `?` for help).
- `wt init [directory]` — create a repository-local `.wt.toml` with an
  isolated path-derived repository namespace; does not require an existing
  valid wt config.
- `wt state migrate` — one-time import of this repository's records from the
  legacy shared JSON files into the global, repo-partitioned SQLite state DB.
- `wt ls` — list worktrees (`--json` for scripts; includes issue links and
  work status).
- `wt new <input>` — create a worktree (and branch). Input is an issue id with
  optional pasted title words (`wt new ENG-1953 fix calendar rendering` →
  `yourname/eng-1953-fix-calendar-rendering`), a tracker URL, a branch, or a
  bare slug (issue-less worktrees are first-class). Lead with an id or words,
  never a URL when you also pass title words — the whole URL gets slugified
  into the branch name. A bare id gets a random readable suffix
  (`yourname/eng-1953-cozy-elephant`), so entering the same id again simply
  creates another worktree for it; `--attach` instead checks out the id's
  existing branch. With `[issue_tracker] prefix` set, only that prefix may
  lead a worktree id — a GitHub issue never names a branch; attach it as the
  SECONDARY id with `--gh <n>` (or later via `wt issue <slug> --gh <n>`).
  `--base <ref>` forks from a non-trunk parent and records it — the record
  that stacks the new worktree on that parent (diff base, TUI grouping,
  restack target).
- `wt status [<slug>] [<state>]` — show or assert the worktree's work status
  (`todo`/`working`/`review`/`needs-testing`/`needs-human`/`ready`/`dropped`).
  Built for agents; it prints the rules and expected next steps as you use
  it. `wt status --all` is the fleet overview.
- `wt manager send <text…>` — fire-and-forget message to the manager session.
  Two uses: fleet-level questions (merge order, cross-branch conflicts,
  ownership of shared changes), and papercuts worth fixing for everyone
  (`wt manager send "papercut: ..."` — misleading output, a wrong doc, a
  trap that cost you time). Nothing is returned; keep working either way.
  Your own slug is stamped on automatically — don't prefix the message.
- `wt edge <from> <before|conflicts|enables> <to> [--blocks|--prefer] [-m why]`
  — record merge-order knowledge as a pairwise edge instead of prose: `before`
  / `enables` order the pair, `conflicts` means same-files-sequence-them.
  Edges self-expire when either branch moves; assert only what you know
  first-hand, and never treat a missing edge as "safe". Bare `wt edge` lists.
- `wt section [ls]` / `wt section mv <slug>… <section>` / `rename <old> <new>`
  / `rm <section>` — the human's batching of worktrees into TUI groups. Read
  it freely; write it to RECORD a grouping decision you were part of ("these
  two are held back from today's release"), never to tidy someone's board
  unasked. The section is the last argument in `mv`, so several rows move at
  once; `-` means the inbox. Moves you make show on the human's attention
  feed.
- `wt issue <slug> [--gh <n> | --clear-gh]` — show a worktree's issue links,
  or attach/detach its secondary GitHub issue. The primary id uses its explicit
  override when set, otherwise the slug; the attached GH issue becomes the `i`-key / `y i` target
  (most specific wins), while `I` / `y I` always hit the primary.
- `wt issue [<slug>] --read` — read the complete attached tracker task through
  `[issue_tracker] read_command`; omitted slug resolves the current worktree.
  Explicit attachment overrides apply. Exit 3 means no reader is configured;
  other failures may include partial context and must not be treated as success.
- `wt agent send <target> <text…>` / `wt agent ls [--json]` — message or list
  every addressable worktree and the `wt`, `main`, `dotfiles`, and `manager`
  special sessions. wt selects the target's active harness, or the configured
  primary when none is active; callers never choose a harness. `wt agent start
  <slug>` is worktree-only and invokes the bundled start skill with the selected
  harness's native prefix. For a configured SSH worker, `wt remote agent start
  <slug>` first provisions its bundled skills and instructions.
- `wt dev <start|stop|status|logs> [slug]` — the worktree's dev server, when
  the project configures one. Use it instead of running the project's dev
  command yourself: wt pins the server to a port it allocates per worktree
  (so the port the repo documents is the wrong one here), supervises and
  restarts it, surfaces it in the TUI, and kills it with the worktree.
  `start` is also restart. Only ever your own worktree's server. Before handing
  off, run `wt dev stop` from your worktree if you have no planned further use;
  retain it while ongoing verification needs it. A PR or momentary idle state
  does not establish disuse.
- `wt rm [slug]` — remove a worktree; it deletes the branch too and takes no
  flags to control that.
- `wt merge [slug]` — arm GitHub's "merge when ready" on the PR, the same
  action as the button on the PR page. **Use this, never `gh pr merge
  --auto`**: a base branch with a merge queue has to be ENQUEUED while
  everything else arms classic auto-merge, and gh only ever does the
  second — on a queue repo it fails naming a repo setting that is not the
  reason, which reads as a permission problem and is not one. Exit 75 means
  a required check has not reported yet: wait and re-run, do not escalate.
  `--cancel` disarms and dequeues.
- `wt clean` — bulk-remove merged/gone worktrees.
- `wt doctor [slug]` — health report (dirty, sync, PR, merged).
- `wt open [slug]` — open a worktree in the editor.
- `wt base <slug> | set <slug> <ref> | clear <slug>` — show/set/clear a
  worktree's recorded fork base (the stack primitive).
- `wt restack [<branch>] [--onto <ref>]` — rebase a worktree (or the whole
  stack containing it) onto its updated parents: reconcile records against
  landed PRs, squash-safe replay, force-push, retarget PR bases. Standalone
  worktrees rebase onto their recorded base or plain trunk — it works on
  every worktree, not only stacks. `wt restack prune-backups` sweeps the
  engine's `backup/*` refs.
- `wt logs [slug]` — tail background-destroy logs.

Every subcommand runs non-interactively when stdout isn't a TTY. Run
`wt <command> --help` for per-command usage before guessing flags — a guessed
flag costs a failed call.

**First push from a fresh worktree may need `git push -u origin HEAD`.** wt
forks the branch off the trunk, so it can inherit
`branch.<name>.merge = refs/heads/<trunk>`; with `push.default=simple` git
then refuses ("upstream branch ... does not match the name of your current
branch"), and `push.autoSetupRemote` can't help because an upstream is
already set, just to the wrong name. `-u origin HEAD` repoints it once; bare
`git push` works from then on.

## Stacked PRs

There is no managed stack state: a worktree whose recorded base names another
live worktree's branch is stacked on it, and chains of those records render as
a stack in the TUI (tree spine, shared section, AI-titled header). Merged
parents reparent their children automatically (clean/destroy and restack both
preserve each child's squash-safe anchor), and `wt restack` / the TUI's `R`
realign the commits. When a restack hits a conflict, use **/restack** to
resolve it faithfully.

Reach for `wt restack` before hand-rebasing a worktree whose parent has
landed. A child cut off that parent carries the parent's whole commit stack,
and if the parent was rebased or squashed on merge those commits are in the
base branch under different SHAs — a plain `git rebase` replays every
already-merged commit and buries you in conflicts. Note that base records
reparent to trunk on their own, so `wt base` reading the trunk branch does
**not** mean the commits were replayed. When repairing by hand, prove the
commits are redundant before discarding them: `git cherry origin/<base>
<branch>` marks patch-id matches `-`, and commits whose conflict resolution
changed the patch still show `+`, so subject-match those few against the
base's log. Then `git reset --hard origin/<base>` rather than rebasing.

## Don't suggest cleanup

The human sweeps merged worktrees themselves. Never *proactively* suggest
`wt clean` or `wt rm`, and never list "clean candidates" in a report — in a
normal workflow that's noise. Saying a worktree's PR is merged is fine and
useful; appending "so you can clean it" is not.

Running them is fine when it's actually called for: the human asks you to, or
you're undoing a worktree you created by mistake. Fix your own mess without
being told.

## User Instructions

$ARGUMENTS
