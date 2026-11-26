# What a worktree owes

A worktree has **one owed list**: everything it still has to do, or wait for,
before it is finished. This page is the model. The commands are in
[cli.md](cli.md#wt-owed---all---json---slug-slug), the rendering in
[tui.md](tui.md#what-a-row-owes), and the reasons the pieces exist in
[fleet.md](fleet.md#the-design-responses).

## Why one list

What a worktree owed used to live in four places: the merge gate, one
post-merge check, the wakes it set for itself, and free-text `OPS` lines in
its ready note. Each was read by different code, so they disagreed: a row
owing a staging check and a production step showed only one, a wake that had
been delivered counted as done whether or not anyone acted on it, and a note
could promise a step nothing tracked.

Now nothing but the pure derivation `wt_core::owed` decides what a row owes.
The board, the details pane, badges, cleanup, removal hazards, JSON and CLI
output all read `owed_items`, and a new surface must too.

## Items

An item has a kind, a "when" (what it waits for), a "what" (its text) and a
state, `waiting` or `due`.

| kind | what it is | set with | closed by |
|---|---|---|---|
| `gate` | the merge gate: merging now would make something worse | `wt status --blocked-on "<why>" [--until <condition>]` | wt, when the `--until` conditions hold; else a person (`wt status --unblock`) |
| `check` | a verification only a deployed stage can prove; one **per stage**, so staging and production can both be owed | `wt status --verify-in-staging "<steps>"`, `--verify-in-production "<steps>"` | `wt done <stage> -m "<evidence>"` or `wt status verified -m`; `--no-verify-in-<stage>` drops it unrun |
| `step` | anything else the worktree comes back for | `wt wake <condition>... -m "<what to do>"` | see [Delivery is not completion](#delivery-is-not-completion) |

- A gate is always `waiting` and is owed only until the row's own work
  lands: a landed row lists no gate. Its `--then` wake is part of the gate
  item until then, and a step of its own after.
- A check is `due` once the change is deployed to its stage (the landing
  reached the stage's branch and, where the stage has a deploy probe, the
  probe reports it) and any not-before time has passed. It is `overdue`
  after the configured days.
- A pending wake is a `waiting` step, even when its conditions held and it is
  only waiting for delivery.
- Watches that only tell the person (`wt notify`, `W`) are **not owed**. They
  never hold cleanup and are listed apart (`wt watch`).

Order: due checks by stage, due steps oldest first, the gate, then waiting
items by what they wait for: staging, production, anything else, a time.

## Delivery is not completion

A wake is a promise to come back. What delivering it means depends on whether
the worktree's own work had landed when it fired:

- **Not landed:** the wake closes on delivery, as coordination does (rebase
  when a parent merges, test at 17:00).
- **Landed, or unknown to the engine** (a PR exists, GitHub data is stale and
  there is no local proof): the wake becomes a **due step**. It stays owed until
  `wt done <id> -m "<what you did and what you saw>"` closes it with
  evidence, or `wt done <id> --again <condition>...` turns it back into a
  wake for another part. The prompt ends with the command that closes it.

A check works the same way. When it comes due wt wakes the row's session with
the steps: the built-in rule `wt.check`, which needs no configuration, or a
configured `status.verification_due` rule that prompts that session and so
replaces it ([automations.md](automations.md#the-built-in-check-wake)). Being
prompted closes nothing; `wt done <stage> --again <time>` keeps the check
owed and not due before that time.

## Phase

Before the worktree's work lands, its asserted status says where it stands.
After landing, the row's **phase** is derived, never asserted:

| phase | means |
|---|---|
| `due` | something is due now |
| `waiting` | something is open, nothing is due |
| `done` | nothing is open |

A command that cannot establish the landing ([how it is
read](cli.md#wt-owed---all---json---slug-slug)) reports it as `unknown`:
nothing is called due and the phase is null. No work state was added for
phases, because an older build reads an unknown state as no status. The
effective state still reads `needs-testing` while any check is due, so
automations and sorts keep their meaning; a due step does not change it.
`verified` is written only when no check remains.

## What holds cleanup

One predicate, `owes_anything`: any open check (due or not), any pending
wake of the row, any due step. Cleanup, `wt clean`, `builtin:clean` and the
`c` review keep such a worktree and say `still owes: <headline> (<when>)`;
`wt rm` refuses without `--force`. A merge gate alone holds nothing, and
neither does a notify-only watch. A landed row in phase `done` is cleanable.

A removed worktree's checks and due steps move onto its removal record:
`wt owed --slug <slug>` shows them, `wt done --slug <slug>` closes them
there, and a restore carries back what is still open.

## Every change prints the list

Each command that changes the list (`wt status` with a gate or check flag,
`wt wake`, `wt done`, `wt watch cancel`) ends by printing it, so the writer
sees the result instead of assuming it:

    This worktree owes:
      due      confirm the logs (due since 11:36) · wt done 93co6q -m "<evidence>"
      waiting  check: 1. check prod 2. check logs (in production)

The ready note therefore carries no phased `OPS` lines: `wt status` refuses
them and says where each kind goes. A note may say `OPS: none` only while the
list is empty.

## Stacks

A stack member whose own work has landed is no longer a parent **for
display**: it is drawn as a single row and members based on it become roots.
So moving a merged head moves only that row, and derived blocks take only
that row. Fork-base records are not rewritten; stack identity, the stack
pause, `stack.parent_merged` and `wt restack` keep reading the full chain.

## Derived blocks

Sections are asserted by people and agents. With `[ui] owed_sections = true`
(off by default) a landed, unarchived local row is instead shown under what
it owes. One function, `wt_core::OwedBlock`, files both the board's rows and
the items `wt owed --all` groups, so the two cannot disagree. A row is filed
by its phase and, while it waits, by its open items, the first that applies:

| block | the row is here when |
|---|---|
| **Due** | something is due |
| **Waiting for staging** | an open item waits for a deploy to staging that has not happened: the row's own stage, another pull request's or commit's (`pr:N:staging`, `commit:<oid>:staging`), or the stage's next release |
| **Waiting for production** | the same for production, when nothing waits for staging |
| **Scheduled** | its next item comes due at a known time: a time condition, a check put off on a stage that has the change, or a delay already running on conditions that hold |
| **Waiting on other work** | anything else: another pull request or worktree merging, a probe |
| **Done** | nothing is open |

Rows under Scheduled are drawn soonest first, and under Due oldest due step
first. A landed row's only remaining meaning is what it owes,
which is the one case where deriving its place is safe. The stored section is
untouched, so turning the option off restores the layout; such a row cannot
be moved or reordered while the option is on.

## Compatibility with older builds

Sessions keep the build that started them for days and share the state.

- **Checks.** The status record gains `checks` (per stage: `steps`, optional
  `notBeforeMs`) and `checksMirror`. The legacy pair `verifyAfterMerge` /
  `verifyIn` stays and mirrors the first open check, staging before
  production. A pure normalize step runs wherever a record is read or
  written: legacy fields present set that stage's check (also the upgrade
  import); legacy fields gone while `checksMirror` names a stage mean an
  older build discharged that stage; a `verified` or `dropped` record owes
  no check.
- **Removed rows** keep `checks` and `owed` on the removal entry itself,
  because an older build replaces the entry's whole status.
- **Due steps** are a top-level `owed` array, not watches: an older engine
  deletes a watch that has no action left. At most 20 per worktree.
- **History** gains the `owed` event, unknown to older builds. No existing
  JSON key changed meaning: `verify_in` and `verify_after_merge` follow the
  legacy mirror, so read `owes` instead.
