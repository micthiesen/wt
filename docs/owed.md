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
  lands: a landed row lists no gate. A status change keeps it. A gate
  asserted on `todo` is a start gate, why the work has not started, and the
  status that starts the work clears it
  ([cli.md](cli.md#wt-status-slug-state--m-note---risk-r---blocked-on-gate---verify-in-staging-steps---verify-in-production-steps)). Its `--then` wake is part of the gate
  item until then, and a step of its own after.
- A check is `due` once the change is deployed to its stage (the landing
  reached the stage's branch and, where the stage has a deploy probe, the
  probe reports it) and any not-before time has passed. wt records when it
  first saw the check due (`due_at` in JSON, an `owed` history event) and
  the check is `overdue` the configured days after that, however often the
  status is asserted again.
- Each stage holds one check: the current pull request's. A worktree can
  carry several pull requests (`wt next`), and a check an earlier one still
  owes is never replaced unseen. `wt next` marks every open check as the
  landed work's: the pull request merged at the branch's head (`forPr`),
  or that head itself where no pull request merged there (`forCommit`).
  Asserting the same steps again takes the check up for the next pull
  request. Other steps in that stage become the next pull request's check,
  and the earlier one becomes a wake step in the same write, waiting for
  `pr:<n>:<stage>` or `commit:<oid>:<stage>` (and for the time it was put
  off to); `wt status` names its id and how to end it, and the history
  says `again when ...: <stage> check: ...`. That wake stays owed once
  delivered whatever the worktree is on by then, due since the check was
  first seen due. A status write that cannot set every such wake (the
  worktree is at its watch limit) is refused whole, before any is set.
  Steps edited in the board's status menu rewrite the check in place: it
  stays the earlier work's and nothing is set beside it.
  A further check for a stage is set by hand the same way:
  `wt wake pr:<n>:<stage> -m "<steps>"`.
- A pending wake is a `waiting` step, even when its conditions held and it is
  only waiting for delivery.
- Watches that only tell the person (`wt notify`, `W`) are **not owed**. They
  never hold cleanup and are listed apart (`wt watch`).
- A [`wt wait`](cli.md#wt-wait-condition---any---timeout-duration---slug-slug---json)
  is not owed either: it blocks the session that runs it and stores nothing.
  It is for what resolves while the session is still working; a wake is for
  what outlives it.

Order: due checks by stage, due steps oldest first, the gate, then waiting
items by what they wait for: staging, production, anything else, a time.

## Delivery is not completion

A wake is a promise to come back. What delivering it means depends on whether
the worktree's own work had landed when it fired:

- **Not landed:** the wake closes on delivery, as coordination does (rebase
  when a parent merges, test at 17:00). The wake an earlier pull request's
  check became is the exception: that work has landed, so it becomes a due
  step like the next case.
- **Landed, or unknown to the engine** (a PR exists, GitHub data is stale and
  there is no local proof): the wake becomes a **due step**. It stays owed until
  `wt done <id> -m "<what you did and what you saw>"` closes it with
  evidence, or `wt done <id> --again <condition>...` turns it back into a
  wake for another part. The prompt ends with the command that closes it.

A check works the same way. When it comes due wt wakes the row's session with
the steps: the built-in rule `wt.check`, which needs no configuration, or a
configured `status.verification_due` rule that prompts that session and so
replaces it ([automations.md](automations.md#the-built-in-check-wake)). Being
prompted closes nothing.

**A due check that cannot be run yet is put off with `wt done <stage>
--again <time>`**: it stays owed and is not due before that time. A separate
`wt wake` for it leaves the check due beside a second item, so `wt wake`
warns when it is set for a stage the row already owes a check in, or with a
prompt that reads like that check. The check's prompt and the `wt owed` line
of a due check both print the `--again` form.

A gate is cleared by exactly the conditions `--until` names. When those are
a time alone, or the gate's text names more than they cover, `wt status`
says so once: `this gate clears <when>; anything else it names is not
checked`.

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

Not owed, but held the same way: a worktree whose checkout has been on
another branch that carries an open pull request, or one GitHub has not
answered for, stays until that branch has its own worktree
([cli.md](cli.md#one-pr-per-worktree)).

A removed worktree's checks and due steps move onto its removal record:
`wt owed --slug <slug>` shows them, `wt done --slug <slug>` closes them
there, and a restore carries back what is still open. Its deploys can no
longer be read, so a check stands by the landing the record holds: base at
removal, production (`landedOn`) when the row's history shows it had landed
there or the history view proved it since. A production check on a removed
row comes due once the record says production. Nobody is left to prompt for
a removed row, so what comes due on one is sent to the manager session
once ([automations.md](automations.md#the-built-in-check-wake)).

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
So moving a merged head moves only that row, and the Landed block takes only
that row. Fork-base records are not rewritten; stack identity, the stack
pause, `stack.parent_merged` and `wt restack` keep reading the full chain.

## The Landed block

Sections are asserted by people and agents. With `[ui] owed_sections = true`
(off by default) the board instead draws every landed, unarchived local row
in **one** derived block, **Landed** ([tui.md](tui.md)), in landing order.
The board has no block per stage: each row in Landed carries a label saying
what it waits for next. A row being removed stays in the block, labelled
`removing` in place of what it waited for, and the header stops counting it
([tui.md](tui.md)); the removal record stores `landed` and `wasArchived` so
the row keeps its place after its checkout is gone.

The headings below exist only in `wt owed --all`, which groups every
worktree's items under them. One function, `wt_core::OwedBlock`, gives both
a row's label on the board and the heading its items are listed under, so
the two cannot disagree. A row is filed by its phase and, while it waits, by
its open items, the first that applies:

| `wt owed --all` heading | the row is here when |
|---|---|
| **Due** | something is due |
| **Waiting for staging** | an open item waits for a deploy to staging that has not happened: the row's own stage, another pull request's or commit's (`pr:N:staging`, `commit:<oid>:staging`), or the stage's next release |
| **Waiting for production** | the same for production, when nothing waits for staging |
| **Scheduled** | its next item comes due at a known time: a time condition, a check put off on a stage that has the change, or a delay already running on conditions that hold |
| **Waiting on other work** | anything else: another pull request or worktree merging, a probe |
| **Done** | nothing is open |

A landed row's only remaining meaning is what it owes, which is the one case
where deriving its place is safe. The stored section is
untouched, so turning the option off restores the layout; such a row cannot
be moved or reordered while the option is on.

## Review stamps

A worktree that has been through a round of a stamping skill
([`[history] stamps`](configuration.md#history-which-skill-uses-a-worktrees-history-records))
carries a stamp. It is not owed and holds nothing; it lives here because it
rides the status record (`work.stamp`: `skill`, `sha`, `at`, `by`), which
every status write carries forward, older builds included. The skill
starting leaves `stampPending` on the slug entry; the first status after it
that is not `working` or `review` earns the stamp at that assertion's HEAD.
A round takes hours at most: a `stampPending` older than 24 hours was left by
a round that was abandoned, so it is ignored and the next status write of any
state drops it.
The stamp stays when the head moves (the marker goes from `✦` to `✧`) and
when the status is asserted again. A new round replaces it;
`wt status --clear` removes it with the status, and a pending one with it.

## Compatibility with older builds

Sessions keep the build that started them for days and share the state.

- **Checks.** The status record gains `checks` (per stage: `steps`, optional
  `text`, `notBeforeMs`, `sinceMs`, `dueSinceMs`, `duePr`, `forPr`; an older build carries
  the ones it does not know through its writes) and `checksMirror`. `steps`
  is the check's text on one line and is what names it: the same steps are
  the same check, in every build. `text` is the same text with its author's
  line breaks, kept only when it has any, and read only while it still
  flattens to `steps`, because an older build that re-sets the steps carries
  the old `text` through. The details pane, the due prompt and the `what` of
  the JSON surfaces give the lines; a list gives the first one. The legacy pair `verifyAfterMerge` /
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
- **Earlier pull requests' checks.** An older `wt next` does not set
  `forPr` or `forCommit`, so a check set after it replaces the one in its
  stage, as before. The wake a check became carries `owedOnDelivery`, which
  an older engine carries but does not read: the engine is the running wt
  (the TUI process), which keeps its build until it is restarted, and one
  from before this delivers the wake to a worktree whose current work has
  not landed and closes it. So each such check is also kept in a top-level
  `keptChecks` array (the wake's id, the check, the step's text), which
  older builds carry untouched. Only a build that knows the key removes a
  record: when the step is closed, cancelled or pushed out, and it follows
  a step that is put off to its new wake. A current engine that finds a
  record with neither its wake nor its due step owes the step again, with
  the same id and due date and a `due: restored, ...` history event,
  once. It forgets the record instead when the worktree is gone or the
  history shows the step closed, dropped or its wake cancelled, which is
  how an older build's `wt done` and `wt watch cancel` are honoured.
- **History** gains the `owed` event, unknown to older builds. No existing
  JSON key changed meaning: `verify_in` and `verify_after_merge` follow the
  legacy mirror, so read `owes` instead.
