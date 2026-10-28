---
name: handoff
description: >-
  Creates a follow-up task in a new wt worktree, writes its prompt.txt brief,
  and starts its agent. Use for /handoff or $handoff, or when the user asks to
  move, fork, or delegate follow-up work into another worktree.
targets:
  - '*'
argument-hint: "<follow-up task, optional tracker id and dependency context>"
user_invocable: true
---

# Handoff

Move one well-defined follow-up into a fresh worktree and start its agent. That
agent runs the `start` skill: it reads `prompt.txt`, deletes it, and owns the
task through testing and status.

An explicit request with a clear task authorizes creating one worktree and
starting its agent. Ask one short question only when the task or its base is
materially ambiguous.

## 1. Check for an owner

Run `wt ls --json`. If a worktree already owns this follow-up, report it
instead of creating another. Update its `prompt.txt` only when it is still
queued and the new text belongs to that same task.

## 2. Create

    wt new [<TRACKER-ID>] <short title words> --no-open [--gh <n>] [--base <current-branch>]

- Give the tracker id only when the user supplied one; a second worktree for
  the same id needs distinct title words. Add `--gh` only for a related issue
  the user supplied.
- Add `--base <current-branch>` only when the follow-up needs this branch's
  unlanded commits; otherwise it forks from trunk. State the dependency in the
  brief either way. `--base` carries commits only: if the follow-up depends on
  uncommitted changes, commit them within the user's authorized workflow or
  ask; never claim they are there.
- Never use raw `git worktree add`. Take the new slug, branch, and path from
  the output.
- Keep the new worktree in the source's section: find the source in
  `wt section ls --json` (entries have `name` and `slugs`; `name: null` is
  Inbox), then `wt section mv --only -- <new-slug> <section>` (`-` for Inbox)
  before launch. Move only the new row. If sections can't be read, keep the
  default and say so in the report.

## 3. Write the brief

Write `prompt.txt` in the new worktree's root, never the current one; it is
never committed. Omit empty sections:

```text
Objective
<the outcome in user terms>

Context and decisions
<facts and decisions the receiving agent cannot cheaply rediscover>

Acceptance criteria
- <observable result>

Relevant references
- <issue, file, command, or related worktree>

Dependencies
<base/ordering/overlap constraints, or "None">

(You can delete this prompt.txt after reading it.)
```

Keep it to this task, not a summary of the conversation, and point to the
issue, plan, or commit that already holds the spec. Never include credentials
or tokens.

## 4. Queue and launch

1. `wt status <slug> todo -m "<one-line goal>"`
2. `wt agent start <slug>`. It picks the configured harness and sends that
   harness's form of `start`; never address a harness yourself.

If launch fails, leave the worktree and brief in `todo`, report the exact
error, and stop; that alone is not `needs-human`. Do not wait for the new task
to finish.

## 5. Report

The goal, slug, branch, path, base when not trunk, and the harness
`wt agent start` selected; confirm the brief is written, the status is `todo`,
and `start` was dispatched.
