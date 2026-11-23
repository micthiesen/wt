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

## 2. Write the brief

Write the brief to a scratch file, never into a checkout; `wt new` copies it to
the new worktree's `prompt.txt`, which is never committed. Omit empty sections:

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

## 3. Launch

One command creates the worktree, installs the brief, files it in a section,
queues it as `todo`, and starts its agent:

    wt new <TRACKER-ID> --slug <short words> --no-open --brief <file> --section "<name>" -m "<one-line goal>" --start [--gh <n>] [--base <current-branch>]
    wt new <Title words in sentence case> --no-open --brief <file> --section "<name>" -m "<one-line goal>" --start [--gh <n>] [--base <current-branch>]

- Give the tracker id only when the user supplied one. With an id, `--slug`
  keeps the tracker's title for the new agent to retitle; a second
  worktree for the same id needs a distinct slug. Without one, the words are
  the slug and the title. Add `--gh` only for a related issue
  the user supplied.
- Add `--base <current-branch>` only when the follow-up needs this branch's
  unlanded commits; otherwise it forks from trunk. State the dependency in the
  brief either way. `--base` carries commits only: if the follow-up depends on
  uncommitted changes, commit them within the user's authorized workflow or
  ask; never claim they are there.
- `--section` is the source worktree's section: find the source in
  `wt section ls --json` (entries have `name` and `slugs`; `name: null` is
  Inbox, written `-`). If sections can't be read, omit it and say so in the
  report.
- `--start` picks the configured harness and sends that harness's form of
  `start`; never address a harness yourself.
- Never use raw `git worktree add`. Take the new slug, branch, and path from
  the output.
- Several follow-ups: run one `wt new … --start` per task concurrently; they
  do not conflict.

If a step fails, the command prints the commands that finish the launch and
the worktree stays as it is; run them once, and if the agent still cannot
start, report the exact error and stop; that alone is not `needs-human`. Do
not wait for the new task to finish.

## 4. Report

The goal, slug, branch, path, base when not trunk, and the harness
`wt new --start` selected; confirm the brief is written, the status is `todo`,
and `start` was dispatched.
