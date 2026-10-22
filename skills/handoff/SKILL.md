---
name: handoff
description: >-
  Create and launch a distinct follow-up task in a new wt-managed worktree.
  Use when the user invokes /handoff or $handoff, or explicitly asks to move,
  fork, delegate, or continue follow-up work in another worktree and start its
  coding agent. Writes a focused prompt.txt, queues the task, and starts the
  configured primary harness; do not use for work that should stay in the
  current worktree.
targets:
  - '*'
argument-hint: "<follow-up task, optional tracker id and dependency context>"
user_invocable: true
---

# Handoff

Move one well-defined follow-up from the current conversation into a fresh
worktree and start its configured primary agent. The bundled `start` skill is
the consumer: it reads `prompt.txt`, absorbs the task, deletes the disposable
brief, and owns implementation through testing and status hand-off.

## 1. Define the follow-up

Extract the objective, acceptance criteria, relevant decisions, dependencies,
and useful file or issue references from the conversation and current repo.
Keep the brief task-focused; do not dump or summarize the whole conversation.

Treat an explicit invocation with a clear task as authorization to create one
worktree and start its agent. Ask one concise question only when the task or
its dependency base is materially ambiguous.

Before creating anything, run `wt ls --json` and avoid duplicating an existing
worktree that already owns the follow-up. If one exists, update its
`prompt.txt` only when it is still queued and the new text clearly belongs to
that same task; otherwise report the collision rather than overwriting another
agent's brief.

## 2. Choose identity and base

- Use `wt new <ID> <short title words> --no-open` when the user supplied a
  tracker id. A second worktree for the same id needs distinct title words.
- Use `wt new <short title words> --no-open` for issue-less follow-ups.
- Add `--gh <n>` only for a supplied related GitHub issue.
- Add `--base <current-branch>` only when the new task depends on the current
  branch's unlanded changes. Independent follow-ups fork from the configured
  trunk. State the dependency in the brief either way.

`--base` transfers committed history only. If the follow-up depends on
uncommitted changes, do not claim they are available in the new worktree;
either establish a durable commit within the user's authorized workflow or
ask how they want that dependency handled.

Always use `wt new`, never raw `git worktree add`. Resolve the created slug,
branch, and absolute path from the command output or a fresh `wt ls --json`.

Create the follow-up in the source worktree's section when possible. Before
creation, identify the source from the current directory and `wt ls --json`.
Read its section with `wt section ls --json`: each entry has `name` and `slugs`;
`name: null` means Inbox. Keep this choice separate from `--base`.

After creation and before agent launch, move only the new worktree with
`wt section mv --only -- <new-slug> <source-section>`. Use `-` for Inbox.
Do not move the source or its stack. If the source or its controller-owned
section is unavailable, keep the default section and state that limit in the
handoff report. Do not ask the user to choose a section for this fallback.

## 3. Write the handoff brief

Create `prompt.txt` in the NEW worktree root, never the current worktree. It is
an untracked transport artifact and must not be committed. Use this compact
shape, omitting empty sections:

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

Prefer references over copied prose when an issue, plan, commit, or existing
artifact already holds the durable specification. Never include credentials,
tokens, private keys, or unrelated conversation details.

## 4. Queue and launch

After the brief is safely written:

1. Run `wt status <slug> todo -m "<one-line goal>"`.
2. Run `wt agent start <slug>`.

`wt agent start` reads wt's configured primary harness, cold-starts that
harness if needed, and invokes its native form of the `start` skill (`/start`
for Claude, `$start` for Codex/OpenCode). Do not address a harness directly or
guess its command prefix.

If launch fails, leave the worktree and `prompt.txt` intact in `todo`, report
the exact failure, and stop. Do not mark it `needs-human` merely because the
agent did not start. A successful dispatch is fire-and-forget; do not wait for
the new task to finish.

## 5. Report

Return the follow-up goal, slug, branch, path, recorded base when non-trunk,
and the harness `wt agent start` selected. Confirm that the brief was written,
the status is `todo`, and the start skill was dispatched.
