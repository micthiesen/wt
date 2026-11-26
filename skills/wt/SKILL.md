---
name: wt
description: >-
  Drives the `wt` worktree CLI: create, list, remove and message worktrees and
  their agents, arm auto-merge, stacked PRs, restacks, sections and merge
  edges, and wait for a time, merge, deploy or release. Use when the user
  mentions wt, a worktree, stacking, restacking, auto-merge or being told or
  woken when something happens.
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
wt new <ID> --slug <s> --brief <file> --section <name> -m <note> --start   # launch a task in one step
wt agent start <slug>                  # start a worktree's agent with the start skill
                                       # --model fable|opus (here or with wt new --start): only when the user names a model
wt agent send <slug|main|wt> <text…>   # the manager: `wt manager send`
wt section mv <slug>… <section>        # section is LAST; `-` is the inbox
wt edge <from> before|enables|conflicts <to> [--blocks] [-m why]
wt restack [<branch>]
wt restack prune-backups [--days <n>]   # delete old backup/restack-* branches
wt merge [slug] [--cancel]             # arm merge-when-ready
wt status ready --risk <r> --blocked-on "<why>" --until <cond>… [--then "<prompt>"]   # a gate that clears itself
wt wake <cond>… -m "<what to do then>" # wake this session then; end the turn
wt notify <cond>… [-m "<text>"]        # tell the person then
wt watch [--all]                       # what is waiting; cancel <id>…, check <cond>…
wt owed [--all]                        # what this worktree (or every one) still owes
wt done [<id>|staging|production] -m "<evidence>"   # close it; --again <cond>… puts it off
wt history [slug]                      # what happened to a row, removed ones too
wt next [branch] --title <outcome> [--issue <ID>|none] [--gh <n>|none]
wt issue [slug] [--gh <n> | --clear-gh | --read]
wt title [title…] [-w slug]          # show, or set any but a person's title
wt rm [slug] [--keep-branch]           # deletes the branch by default
wt archive <slug> / wt restore <slug>  # file a row under Archived, or return it; checkout untouched
```

## Rules

- **Naming a new worktree.** Lead with the tracker id:
  `wt new ENG-1953 --slug calendar-week-view`. Pass the id, never a URL. The
  tracker supplies the title, which the worktree's agent replaces with its
  outcome (`wt title`). Title words after the id
  (`wt new ENG-1953 Fix calendar`) set the first title; a bare id gets a
  random slug suffix (another worktree for that id). Without an id, the words
  alone make the slug and title. Write title words in sentence case.
  `wt title` shows or sets the title; nothing generates one. With `[issue_tracker] prefix` set, only
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
- **Waiting.** Never sleep, poll or set a harness timer for something wt can
  observe: `wt wake` it and end the turn, `wt notify` it when the person
  asked to be told, `--until` it when it is a merge gate. Conditions are one
  argument each and all must hold: a time (`17:00`, `5pm`, `3h`,
  `"tomorrow 09:00"`); this worktree (`merged`, `staging`, `production`,
  `ready`, `green`, `verified`, `closed`); a pull request (`pr:3806`,
  `pr:3806:production`); another worktree (`wt:<slug>:merged`,
  `wt:<slug>:gone`); the next release (`release:production`); a configured
  probe (`check:<id>=<arg>`). All but a time take a delay that must hold
  unbroken: `green+1h`, `release:production+24h`. Write `pr:3806`, never
  `#3806`: the shell reads `#` as a comment. `wt watch check <cond>…` says
  what a condition means here before you set it; `staging` and `production`
  mean deployed only where the repository has a deploy probe, otherwise
  landed on that stage's branch. `--slug <slug>` sets one for another
  worktree. The wt TUI evaluates watches, so nothing fires while it is
  closed; the commands say when it is. A wake delivered after the worktree's
  own work landed stays owed until `wt done <id> -m "<evidence>"` closes it.
  `wt owed` lists what a worktree owes (gate, checks per stage, steps), and
  anything but a gate keeps it from cleanup; a notification about a landed
  worktree outlives it.
- **Another PR in the same worktree.** After its PR merged, run
  `wt next --title "<outcome>"` there instead of a new worktree or a hand
  reset, then push as below. `--issue <ID>` alone takes the tracker's title.
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

- Status and coordination: `status`, `owed`, `done`, `wake`, `notify`, `watch`, `history`,
  `fleet`, `manager`, `hold`, `edge`, `guide`, `automations`
- Worktrees: `new`, `next`, `restack`, `merge`, `ls`, `rm`, `clean`,
  `archive`, `restore`, `open`, `doctor`, `logs`, `base`, `issue`, `section`
- Agents and servers: `agent`, `dev`, `perf`, `claude`, `codex`, `remote`,
  `auth`, `controller`, `events`, `stages`
- Setup: `init`, `state migrate`, `skills`, `install`, `update`, `rollback`,
  `version`

## User Instructions

$ARGUMENTS
