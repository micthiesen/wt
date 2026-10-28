---
name: wt
description: >-
  Drives the `wt` worktree CLI: create, list, remove and message worktrees and
  their agents, arm auto-merge, stacked PRs, restacks, sections and merge
  edges. Use when the user mentions wt, a worktree, stacking, restacking or
  auto-merge.
targets:
  - '*'
---
# wt

`wt` keeps many Git worktrees in flight, one branch and coding-agent session
each, on a TUI board (`wt` alone) and a CLI. Every subcommand runs
non-interactively outside a TTY. Check `wt <command> --help` before guessing a
flag. Invoke `wt` by name so session shims apply.

## Cheat sheet

```text
wt ls [--json]                         # rows with PR, status, issue links
wt new <ID> [title words…] [--base <branch>] [--gh <n>]
wt new <ID> --attach                   # check out the id's existing branch
wt agent start <slug>                  # start a worktree's agent with the start skill
wt agent send <slug|main|wt> <text…>   # the manager: `wt manager send`
wt section mv <slug>… <section>        # section is LAST; `-` is the inbox
wt edge <from> before|enables|conflicts <to> [--blocks] [-m why]
wt restack [<branch>]
wt restack prune-backups [--days <n>]   # delete old backup/restack-* branches
wt merge [slug] [--cancel]             # arm merge-when-ready
wt next [branch] [--issue <ID>|none] [--gh <n>|none]
wt issue [slug] [--gh <n> | --clear-gh | --read]
wt title [title…] [-w slug]          # show, or set once over an issue title
wt rm [slug] [--keep-branch]           # deletes the branch by default
```

## Rules

- **Naming a new worktree.** Lead with the tracker id, then the title words:
  `wt new ENG-1953 fix calendar rendering`. Pass the id, never a URL. The
  words become the worktree's title as typed; a bare id gets a random slug
  suffix (another worktree for that id) and the tracker's title. Without an
  id, the words alone make the slug and title. `wt title` shows or sets the
  title; nothing generates one. With `[issue_tracker] prefix` set, only
  that prefix may lead; a GitHub issue never names a worktree, so pass it as
  `--gh <n>` next to a slug.
- **Stacking.** `--base <branch>` on `wt new` stacks the new worktree on that
  branch; `wt base` shows or edits the record. After a parent lands or trunk
  moves, run `wt restack`, never a plain `git rebase`: the parent's commits
  are in trunk under new SHAs. `wt base` reading trunk after a parent lands
  does not mean the commits were replayed. On a conflict, use the `/restack`
  skill. Before discarding commits by hand, prove they landed
  (`git cherry origin/<base>`; `+` lines need a subject match against the
  base's log).
- **Merging.** Use `wt merge`, never `gh pr merge --auto`: only `wt merge`
  enqueues on a merge-queue base, and gh's error there looks like a
  permission problem that isn't one.
- **Another PR in the same worktree.** After its PR merged, run `wt next`
  there instead of a new worktree or a hand reset, then push as below.
- **First push.** If `git push` says the upstream branch does not match the
  current branch, run `git push -u origin HEAD`. Never push to the
  `HEAD:<trunk>` it suggests.
- **Rift backend.** Under `[backend] kind = "rift"` each worktree is a separate
  clone with its own stale remote refs, which `wt doctor`'s sync column also
  reads. Run `git fetch` in that worktree before any ahead/behind or
  `origin/<base>` comparison.
- **Sections.** Sections are the human's grouping. Write them to record a
  grouping decision you were part of, never to tidy the board.
- **Cleanup.** The human sweeps merged worktrees. Never suggest `wt clean` or
  `wt rm` or list cleanup candidates; run them only when asked or to undo a
  worktree you created by mistake. `wt rm` refuses dirty or unpushed work.
  Report what would be lost and pass `--force` only after the user says to
  discard it; a request to remove the worktree is not that.

## Index

- Status and coordination: `status`, `fleet`, `manager`, `hold`, `edge`,
  `guide`, `automations`
- Worktrees: `new`, `next`, `restack`, `merge`, `ls`, `rm`, `clean`,
  `archive`, `restore`, `open`, `doctor`, `logs`, `base`, `issue`, `section`
- Agents and servers: `agent`, `dev`, `perf`, `claude`, `codex`, `remote`,
  `auth`, `controller`, `events`, `stages`
- Setup: `init`, `state migrate`, `skills`, `install`, `update`, `rollback`,
  `version`

## User Instructions

$ARGUMENTS
