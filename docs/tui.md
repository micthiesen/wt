# TUI guide

Run `wt` with no arguments to open the terminal interface. Press `?` for the
live keymap; use `/` to filter help. The help text is rendered by the native
application and is the most current reference.

## Layout

The list takes 44% of the width, clamped to 32..52 columns, and the right
column holds details above the activity feed. Details get at most 20 rows; the
feed gets the rest and at least 7. Below 60 columns the panes stack vertically.
With `[ui].activity_pane = "full_width"`, list and details share the top region
and the activity feed spans the bottom with at most 12 lines of content, so a
taller terminal grows the list and details instead. Terminals smaller than 20 columns or 5
rows show a resize prompt. The UI uses a Nord-derived palette and Nerd Font
glyphs; a terminal font without Nerd Font symbols shows placeholder boxes.

Each section starts with a dim rule naming it, separated from the previous
section by a blank line. A row shows, left to right: the stack rail (tinted per
stack lane), the work-status marker, the title exactly as named (issue
identity appears in details, never as a title prefix), a dim `→ Section` when a stack member sits apart
from its parent's section, and a right-aligned glyph cluster. The cluster's
fixed order is what the row owes and its bell (see [What a row owes](#what-a-row-owes)), action
running, dirty, rebase or conflict, deploy, AI session
state, review bot, review decision, PR state or merge-queue position, and CI
checks. `[ui].hidden_badges` removes slots by name (`watch`, `action`, `dirty`,
`rebase`, `deploy`, `session`, `review_bot`, `review`, `pr`, `checks`).
Archived rows render dimmed. Review requests and the archive sit at the bottom
of the list. The cursor keeps three rows of context while scrolling and the
list border carries a scrollbar when rows overflow. When the board changes on
its own (a refresh adds, removes or moves rows), the cursor's row stays on the
same screen line, and a viewport scrolled with the wheel keeps its top row.

Work-status markers use color for state: red for needs-human, yellow for
needs-testing or blocked, cyan for working, purple for review, green for ready
or verified, and a dim hollow dot for todo or unset. A slash marks a gated or
dropped status. A hollow marker in a state's color means the saved status SHA
differs from the current observed HEAD, and overdue post-merge verification is
red. A yellow hourglass replaces the marker while a
worktree initializes: from the creation request until its checkout exists, and
then while the dependency install that creation started is still running. The
ordinary marker returns when the worktree is ready for a session. A row with a review stamp ([`[history] stamps`](configuration.md#history-which-skill-uses-a-worktrees-history-records)) wears a star in place of its status circle, ✦ while HEAD is the stamped commit and ✧ once it has moved; gated, dropped, initializing and landed rows keep their own marker, and the details status line ends `· ✦ <skill>`. A landed row's marker follows what it still owes, not its asserted state: a yellow dot while something is due (red once anything is overdue), a dim dot while everything it owes waits, and the green merge glyph when it owes nothing. Landed
means the branch's own commits reached the base: its PR merged at this HEAD,
or at a head this checkout holds only copies of (rebased onto a newer base,
or with the base merged in, after the push), or its HEAD is in the base and
was either merged in or committed on the branch itself. A
worktree only fast-forwarded (or reset) to a base commit is never landed, even
when its recorded fork anchor is older. The same
answer drives the glyph and `c`, and `c` never offers a row whose status is
working or needs-human.

Sections are explicit: one lasts from creation (`l n`) until you delete it with
`D`, and stays on screen when its last worktree leaves, drawn as its rule
alone. That rule is the section's cursor stop, so an empty section
can be renamed, deleted, moved with `J`/`K`, or filled (`n` creates into it).
Deleting a section that still has worktrees asks first and moves them to the
Inbox. The Inbox and the archive appear only when they have rows.

Sections fold with `Tab`. Only folded and empty sections are cursor stops; an
expanded section is just its rows. A folded header reads `[×NN] Name` with a
right-aligned count of each marker its rows wear, most urgent first, so it
shows exactly the glyphs you would see expanded (dim markers such as todo,
finished and archived are left out), plus conflicts and failing checks. Selecting it shows the full rollup and blocker notes (for the Landed block, [what its header counts](#the-landed-blocks-pane)) in the
details pane. `[ui].sort = "status"` orders rows by work status; manual section
placement remains separate. Within a section, rows without a manual order sort
first, and the rows of a stack drawn in the same section sort as one unit,
placed by the shallowest of them. A stack member
whose own work has landed is drawn as a single row and its children as roots,
so moving a merged head (`l`, `J`/`K`) moves only that row.

With `[ui] owed_sections = true` (off by default) every landed, unarchived
local row is drawn in one derived block, **Landed**, instead of its section.
The block sits after the manual sections and the Inbox and before the
archive, is hidden while empty, and folds like a section. Its header counts its rows by where they stand, leaving out a count
of zero, open or folded; only `N due` is coloured, as its rows' own `due`
labels are (red once anything is overdue). Folded, the name is on the left
and the counts are at the right edge (`[×29] Landed      3 due · 24 waiting ·
2 done`); it shows no marker tally, since a landed row's marker says only
what the counts already do. The counts outrank the rule: a pane too narrow
for the whole header first shortens and then drops the rule's dashes, then
says `wait` for `waiting` (`Landed · 3 due · 24 wait · 2 done`), and the
folded header last drops the name (`[×29] 3 due · 24 wait · 2 done`). A
count is cut only in a pane narrower than that shortest form.

    ── Landed · 3 due · 24 waiting · 2 done ──────────────
       Renew the add-on expiry             due staging  …
       Retire the Edge fallback             production  …
       Rerun the renewal check               Mon 10:53  …
       Drop the old topic sweep                   done  …

Rows are in landing order, oldest first: a row never moves inside the block
when what it owes changes or on a later refresh. The landing time is the one
the row's first `landed` history event recorded: the pull request's merge
time where GitHub gave one, else the committer time of the base commit that
brought the branch in (the merge commit on the base's first-parent line, or
the branch's own commit when the base fast-forwarded to it). So several
merges that wt first sees in one refresh are still in the order they merged.
An event from before that was recorded, or with neither known, counts as
landed when wt saw it, which also breaks a tie; then the slug does. A row
with no `landed` event is placed by the newest commit of its branch, then by
when its checkout was created. History loads after the
first frame, so until it has loaded every row is placed by its last commit
time and the order can change once when the landing events arrive.

Each row says, dim and right-aligned just left of its glyph cluster, what it
waits for next: `due staging`, `due production` or `due` (yellow, red once
anything is overdue), `staging`, `production`, the time its next item comes
due (`22:32`, `Fri 18:12`), `other work`, or `done`. The label is the
heading `wt owed --all` lists the row under
([how a row is filed](owed.md#the-landed-block)), so the two agree. The label
and the glyph cluster outrank the title: a long title is cut with an
ellipsis to make room for them, and the label goes only in a pane too narrow
for it and eight cells of title.

With the option on there is no **Removing** section. A row being removed
stays where it was drawn: in the Landed block, in its own section, or in
Archived if it was archived before the removal. Its title is struck through
while the removal runs. In the Landed block its label says `checking removal`
until the worker has recorded the removal, then `removing`, or `removal
failed` in the error colour, in place of what the row waited for, and the
header stops counting the row.

#### The Landed block's pane

Selecting the folded block shows what its header counts and nothing else.
After the name and the counts (`1 due · 4 waiting · 1 done`) come the groups
that have rows: `● due`, each row followed by one line per thing due (`staging
Import preview renders`, `since 19:34  delete the flag`); `◷ waiting for
staging`, `◷ waiting for production`, `◷ scheduled` (with the time its next
item comes due at the right edge) and `◷ waiting on other work` (with what it
waits for); and `✓ done · c cleans up`. There is no rollup of asserted states
or risks, no attention count and no "blocked on" group: those describe rows
before they merge.

The block cannot be renamed, deleted or used as a move target, and a row in
it cannot be moved or reordered: the notice reads `Landed is derived from
what landed rows owe; set [ui] owed_sections = false to file rows by hand`.
The row's stored section is untouched, so turning the option off restores
the layout. A section whose rows are all drawn in the block keeps its
header, which says so: `Shipped · 1 landed, shown below`.

The details pane's border names the slug. One blank row always separates the
border from the title, which wraps in full; only a title a person set is
tagged, with a dim `(user)` (see [Worktree titles](#worktree-titles)).
The work-status banner follows: a header line, then the note behind a rail in
the state's color. Until the row's own work
lands the header is the asserted state (`blocked · ready`, or `blocked ·
working` and the like for a gate carried onto another state, with the gate's
slash while a gate is set), risk, age, `commits since` when the status
predates HEAD, and `owes N` when the row owes anything: the list itself
follows the labeled rows (see [What a row owes](#what-a-row-owes)). `REVERT`,
`IF WRONG`, `UNTESTED` and `OPS: none` note fields
align under a hanging indent; any other `OPS` text is prose like the rest,
because what a worktree owes is in its list, not its note.

Labeled rows follow, with dim right-aligned labels. `[ui].rows` controls their
order and visibility: `branch` (with its landing base), `issue` (also
`linear`), `stage`, `dev`, `pr` (number, merge queue or auto-merge, checks with
failing names, review, review bot, and with `[guides] enabled` the change
guide last), `claude` (every Claude, Codex, OpenCode, and Pi
session, the F12 target first), and `git` (dirty or clean, diff against the
merge-base, last commit and creation ages, upstream counts in parentheses, base
counts in brackets). The pane no longer shows a `path` row: the id is ignored
where it is still listed, as unknown group names are. On narrow panes the `pr` and `git` rows drop their least important
parts first. When GitHub removed the pull request from the merge queue after
wt armed it and nothing holds the merge, the `pr` row says `removed from the
merge queue: <reason>` in the warning colour, with GitHub's own word for the
reason (`removed` in a narrow pane). It is the rule `wt wait` and `wt fleet`
use, and it is shown only from a live GitHub read. A GitHub or host error
remains visible as an error, and an unavailable fact shows `—` rather than
reading as clean.

Below the rows come a rebase block when the branch would not rebase cleanly
onto its base (listing up to eight conflicting files from a `git merge-tree`
pre-flight), paused automations, the session summary, PR comments, and the
unresolved thread count (`+N or more unresolved threads` when GitHub holds
more threads than one read returns, so N is a lower bound). The status line wraps only between its ` · `
separated items, so a continuation line starts under the first line's text
and never with a separator. A folded section's details show its rollup, member
rows, a red "blocked on you" group with each needs-human member's note, and a
yellow "blocked on" group with external gates. A review request's details show its state, branch,
author, checks, and keys. The details pane scrolls with `Ctrl+J`/`Ctrl+K` and
shows a scrollbar when it overflows.

Creation uses the section where the name prompt opened, including Inbox.
The new row appears below the selected row or stack. A folded section opens
and receives the new row at the top. This position is stored on the controller,
including for a remote checkout. An unstarted row keeps this position across
restarts. Under status sorting, its work status takes precedence once
assigned. Until then the row keeps its place behind the row it was created
after through `J`/`K` presses, its own and its neighbours', and takes a
stored order only when it crosses into another section. Under manual sorting
the first reorder in a section stores an order for every row drawn there, in
the order drawn, so rows still at their creation position stay where they
are. A new row also keeps its drawn place when the row it was created after
is moved, archived, lands, is being removed or is gone, and removing a row
does not change the order of the rows that remain in its section. One limit under
status sorting: when the followed row moved to another section, the new row
is drawn behind the row with the nearest stored order, which need not be the
row that was drawn before it. Deleting a section moves its rows
to the Inbox in the order they had. An existing worktree keeps
its section when its name is entered again.

Creation immediately selects one optimistic row with its title and the
initializing hourglass. Normal list navigation, attention controls and default-harness switching
remain available. The
chosen host resolves the exact checkout identity before creation starts, so
inventory replaces that row without a duplicate. Navigation away is preserved
when preparation or creation finishes. Worktree actions, including F12, become
available when the real checkout is ready; F12 is never queued automatically.
Dependency installation continues as a background action; the row keeps the
hourglass until that action ends, whatever its outcome. Its details git line
reads `waiting to install` while the install is queued for one of the three
install slots, then `installing dependencies`. A removal request
keeps the row in place with a dim gray, struck-through name while safety
checks run. When the worker accepts removal, the row moves to **Removing**, directly
above **Archived**, and file cleanup continues in the background. With
`[ui] owed_sections` there is no Removing section and the row stays where it
is drawn ([the Landed block](#the-landed-blocks-pane) describes its label);
what follows about Removing rows applies to such a row in place. Active
removals reject duplicate requests, and session entry on that row (F10, F11,
F12, Shift+F12, `;` and their pickers) says **removal is already in progress**
instead of queuing behind the teardown. Failed rows stay in Removing with an
alert marker in place of the trash marker; the details git line wears the same
marker and reads `removal failed`, and the details and attention panels show
the error.
Press `d` to inspect the failure and review a retry; the host repeats all
safety checks. If checkout identity is missing, the menu permits inspection
only. When the failed record's checkout is no longer in Git inventory, `d`
instead offers **Dismiss failed removal**, showing the error. Dismissal
forgets only that record: it deletes no files or branches, so delete any
leftover directory by hand. The host refuses it as stale unless the record
is still the same failed job, path and branch, and the checkout path is
absent and unlisted. Removing rows survive restart, even after partial deletion. A row leaves
the list only after the worker confirms success and inventory drops it. A request
cancelled before worker startup restores the normal name style. Delayed hazard reviews are queued separately for each checkout
and host. They appear one at a time after the current menu closes; navigation
does not discard them, and neither does a dialog that another request opens
over one. Each confirmation still rechecks its checkout witness.
A confirmation you did not ask for (a hazard review, a sign-in prompt, or a
"Stop …?" prompt answering a palette key) ignores Enter and `y` for its first
400 ms, with a notice, so a key meant for the board cannot confirm it. Cancel
and scroll keys work at once. A confirmation reopened after the host
refused it as changed keeps `Changed since you looked: …` as its first line.
While it still names what was refused it reads `Reading what changed. This
updates, and takes Enter, in a moment.` and takes no commit; it is rebuilt
when the board shows something newer and then waits the 400 ms, closes with
the notice when the newer board does not offer it, and takes the commit
after 5 seconds if the board has not changed. Two rules guard the board for
400 ms after a layer you expected is not there. When a key asks for a menu
and gets a notice instead, keys that act are ignored, `q` and Ctrl+C
included, and the footer adds `key ignored: no menu opened`; keys that only
move or scroll, the mouse and Esc pass. When a confirmation closes by commit
or cancel, only a second Enter, `y`, `n` or `q` is absorbed (`key ignored:
the prompt just closed`); every other key acts at once. A layer opened by a
key ends the window, and so does Esc. A confirmation that opened while you were in a
session waits again when you return.
Commands taking more than three seconds show elapsed progress. A queued command
names the conflicting operation or capacity wait; this is progress, not a
completed request. Independent controls do not wait for checkout creation.

### Cached, pending and failed states

At launch every pane shows the last data wt saved for it, so the board is
populated before the first fetch. Each pane's title names its state the same
way: `GitHub cached 3m` while that domain shows saved data, or
`MaxBook GitHub cached 3m` for a remote host in the fleet summary,
`loading…` before any data exists, `↻` while a fetch takes a moment, and
`<source> failed` when a fetch failed (the last good data stays visible; saved
data that also failed to refresh reads `GitHub cached 3m · GitHub failed`).
The age belongs to that saved domain, not the whole board or its last refresh.
A successful read clears its cached marker even when the data is unchanged;
a failed or skipped read does not. Unused tracker answers do not mark the list
stale. The list summarizes every host; a row's details reflect only that row's
host. The top bar does not repeat source freshness labels from pane borders.

Every change you make appears at once. Worktree rows contain their title,
stack references and status glyphs only, never issue prefixes or status text.
One fixed-width pending glyph marks `…` while a request is in flight, `↻`
once accepted, or `?` when its outcome is unknown. Uncertainty takes precedence
when several changes are pending. Full explanations, such as
`? arming merge (unconfirmed)`, stay in the details title. Unknown action
outcomes and expired updates also enter the attention feed. Board-wide changes,
such as switching the primary harness, show progress in the bottom status line.
Uncertain board-wide outcomes stay in Attention, without replacing the top bar.
A failed request
rolls back at once with an error toast. A failure or notice from an earlier
request of yours is shown even while a later one is pending; the failure then
stays beside that request's progress and result until your next action. The
board's own background reads never interrupt. A change that a fresh read still does
not show after its deadline (30 seconds for GitHub, reviews and issue statuses;
12 seconds for local state; 10 minutes for creating a worktree and 30 for
removing one) is dropped with a notice; nothing is resent automatically. While
no read that started after the change has finished, a GitHub change waits up
to 120 seconds and a local one up to 60. A key that turns out to
have nothing to do ends its marker at once. Toggles act on what the row shows, and GitHub writes are refused
if the PR or its head commit changed since you saw it.

The bottom pane shows the attention feed, all activity, session output, or
tracked action output; its title names which, such as `attention · 2 new` or
`slug · claude / name · live · 1/3`. Rows show a dim local time, the source,
and the message. Attention carries status changes and section moves made
elsewhere (agents, the manager, another process); your own moves in this TUI,
including rows a deleted section returns to the Inbox, and statuses you set
with `u` appear only as the toast and in all activity. Attention entries wrap with a hanging indent; while following, the newest entry ends on the bottom row and an entry cut off at the top shows its first row (time, source, text cut short) in place of the cut row. The full feed is
one line per event, colored by level. Entries already marked seen are dimmed
above a `seen HH:MM:SS` rule; when nothing new follows it and the feed overflows
the pane, the rule takes the bottom row, and an empty attention feed reads "nothing needs
you". The attention watermark is saved and `x` or `X` marks the current feed as seen.
`Ctrl+E`/`Ctrl+Y`, `Ctrl+Shift+J`/`Ctrl+Shift+K` where supported, and the
mouse wheel scroll the output pane. Selecting an output with `'` pins it until
it ends; `[`/`]` cycle outputs, `"` toggles attention/all activity, and `Esc`
returns to the default. The feed is restored from the app log on startup.

The header shows worktree and archive counts, refresh motion, paused or queued
automations, a dim `↑` notice when a newer wt release is available or installed
and awaiting a restart ([updates.md](updates.md)), GitHub's GraphQL budget once
less than a quarter is left (`gh 12% left (23m)`, warn-colored; see
[github-events.md](github-events.md#graphql-budget)), the primary harness's rate-limit windows (colored as they near
the limit, with time to reset), and the primary harness glyph. Claude's
windows come from `~/.cache/claude-statusline-usage.json`, written by a
statusline outside wt, and read like Claude Code's status line:
`5h 52% (5m) · 7f 20% · 7d 20% (1d13h)`. The order is the session window, each
per-model weekly (`7` plus the model's initial, busiest first), then the
account weekly; a window the plan lacks is left out, countdowns use the two
coarsest units, and a countdown repeated by a later window shows only on the
last. A reading over 30 minutes old turns dim and ends with
`as of <age> ago`, and a window it saw reset shows `?` instead of `0%`. Refreshing uses
a traveling bar wave in a reserved 12-cell slot, so counts and
right-aligned telemetry stay put as refreshes start and finish. The wave follows
the number of refreshing source lanes across hosts and eases to rest. Its 10 Hz
timer stops when idle or covered by a modal; animation only repaints the header.
The footer shows
the active prompt, a toast (failures in red with a glyph), a source error,
board-wide action progress, or a
static key hint, and on the right the manager, main, wt, and dotfiles slot
sessions as `[m] [.] [,] [/]` colored by session state. The manager Claude
session also shows context occupancy after a transcript turn provides usage.
These are status labels, not clickable buttons.
Physical terminal presence is separate from conversation identity. A live Codex
terminal can show its explicit working or action-required title signal before
wt knows its thread UUID. A live terminal without a reliable activity signal
uses the unknown-state color rather than the absent-terminal muted style. Title signals
never assign history, route messages, or establish that automation may act on
an idle session.

Modals share one frame: a rounded border (yellow for destructive confirms), a
title, and their keys listed along the bottom edge. Pickers show chords or
digits in a key column and a `›` cursor. Confirm hazards are red. Help (`?`)
groups the keymap by area and ends with a glyph legend built from the same
rules the list uses; `/` filters with a match count and highlights hits.

History rows are prepared in the background and retained when the pane closes.
`h` shows them immediately while tracker and landing facts refresh.
When `h` opens removed-worktree history, the list pane reads `removed (N)`
and groups rows under day rules, each day starting at 04:00 local time. A row
shows a marker (the work-status color when a landing is proved, otherwise
merged, closed, dropped, gone, open, or a dim trash glyph), the title, pause,
issue and PR glyphs, and a right-aligned age. Older records can be checked
against a bounded local GitHub-merge and production-history scan while history
is open. A landing only moves forward: once a removal is proven in
production its record keeps that (`landedOn`), so a later refresh or restart
never shows it back at the base. The `removal record` pane shows removal time, host, where a landed row stands as `work` (`due`, `waiting` or `owes nothing`, in place of the state it asserted before merging; a row that never landed keeps its saved status),
what the record still owes (`owes`, one line an item, each ending with the command that closes it there: `wt done <id> --slug <slug>` for a due step, `wt done <stage> --slug <slug>` for a check), landing, notes, and the row's recorded history when it has one. `Enter` restores (a restore of a branch that still exists keeps its stack base and owed verification), `H` opens the row's history modal as on the board, `p` opens the PR, `i` opens
the issue when known, and `y` copies the branch. `j`/`k`, `g`/`G`, and `PgUp`/`PgDn` move the cursor, `Ctrl+J`/`Ctrl+K` scroll the record pane, `r` refreshes, and `Esc` or `h` returns to the board. The activity pane's own keys also work: `x` marks attention seen, `"` toggles the attention and firehose feeds, and `Ctrl+E`/`Ctrl+Y` scroll it. Global keys (help, performance, create, manager and slot sessions, automations pause) work as on the board, and `q` quits the app. `p` and `i` toast when nothing is recorded, and `O`
opens the main clone as it does on the board.

### More than one pull request

A worktree is for one branch and one pull request. When its checkout has
been on another branch that carries an open pull request, or one GitHub has
not answered for, the details pane says so in one warn-coloured line:
`Holds more than one PR: also draft #12 on <branch>, <branch> (PR unknown)`.
That pull request has no row, so nothing else on the board shows its checks
or reviews, and the worktree cannot be removed until the branch has its own
worktree ([cli.md](cli.md#one-pr-per-worktree)).


### What a row owes

A worktree has one list of what it still owes ([owed.md](owed.md)): its
merge gate, the check owed in each stage, and the steps it comes back for.
The details pane lists it after the labeled rows and one blank line, always
in full: nothing folds, and the pane scrolls (`Ctrl+J`/`Ctrl+K`, the wheel)
to its last line. Each item is a line with its mark, its "when" and one
detail at the right edge, then all of its text in the same column as the
"when". The lines below show every form; the gate is listed only until the
row's own work lands, so a real row never shows it beside a landed header:

    ● since 11:36                                          wt done 93co6q
      confirm prod logs, then open the PR retiring the old path
    ○ staging                                                     3 steps
      Chat topic sweep and queue run on the Worker crons.
      1. post three 50+ char messages in one channel of a brand with topics
      2. after the next :07/:22/:37/:52, entity_topics rows exist for the
         brand and the queue table is empty again
      3. the Worker log shows one sweep and no retries
    ○ production +7d                                                in 6d
      retire the Edge function fallback
    ○ Fri 18:12 · by manager                                    in 1d 21h
      rerun the renewal check
    ? check vault · exited 2: vault sealed
      rotate the key
    ○ merged                                               before release
      Before the release carrying #3879: confirm the add-on route.
    ⊘ before merge
      prod secret missing
      clears when ○ #3806 in production

- **Order.** What is due first, then what waits for staging, for
  production, for a known time (soonest first), for anything else, and the
  merge gate last. The watches that only tell you follow as dim bell lines.
- **Mark.** A filled dot in yellow once the item is due (red when overdue),
  the gate's slash, `?` while a condition it waits for cannot be read, and a
  dim hollow dot while it waits. That an item is due is its mark and the
  color of its "when", never a word.
- **When.** As few words as place it. A check,
  or a step waiting for the row's own stage, is the stage: `staging`,
  `production`, with a delay `production +7d`. A condition on another pull
  request or commit is `#3889 production`, `#3889 merged`, `be228d9
  production`; on another worktree `<slug> merged`; on a
  release `production release`; on a probe `check <id>`. A time is `22:32`
  today, `Fri 18:12` within six days, `Dec 2 09:24` beyond. Several
  conditions are joined with `&`. A due step reads `since 19:34` and the gate
  `before merge`.
- **Detail.** One thing, dim, right-aligned to the pane, the first of:
  `overdue` (red), `awaiting deploy` while a check's stage does not run the
  landing yet, `wt done <id>` for a due step (a pointer: the command also
  takes `-m`), a countdown for anything whose time is known (`in 6d`, `in 1d
  21h`, `in 40m`: a time, a check put off, a delay that is already running),
  `N steps` for text with numbered steps, and `before release` for a step
  set with `--before-release`.
- **Tails.** Short dim words after the "when": `before release` when the
  detail says something else, `by <who>` for a setter that is neither
  the row's own agent nor you, `delivery not confirmed` for a wake sent
  without proof it arrived, `delivering` once the conditions held, `held:
  automations paused` while a pause holds the delivery, and the reason a
  condition cannot be read.
- **Text.** All of it, wrapped under the "when", with the author's line
  breaks kept: a check's opening line and then why only that stage can prove
  it, or a step's prompt; then its numbered steps (`STEPS: 1. … 2. …`), each
  wrapped under its own text. A check set by a build from before line
  breaks were stored, or restated by one, is one line and reads as one paragraph before
  its steps. A gate's text is followed by a dim line
  saying what clears it, `clears when ○ #3806 in production` (`clears at
  17:00` for a time, `, then wakes the session` with a `--then`), or `a
  person clears it`.

A pane too narrow to set the detail apart puts it after the "when". Times
are in this machine's clock, and countdowns are refreshed at least every 30
seconds while anything waits.

Once the row's own work has landed, the header reads from the list instead of
the asserted state, and says nothing else: `● due` for one thing due, `● due
· 1 waiting` when more waits behind it, `● 2 due · 1 waiting` for several
(the lines below say what), `◷ waiting for staging · 2 owed` (or `waiting for
production`, `scheduled`, `waiting on other work`, as
`wt owed --all` heads them), or `✓ done`. The claim's
risk and age are not repeated: they were about a merge that has happened. A
state that still asks for someone follows the phase (`· working`, `·
review`, `· needs-human`). A row with nothing owed lists nothing.

In the list, the first slot of the badge cluster (`watch` in
`[ui].hidden_badges`) carries one mark for what the row owes: a filled dot
while something is due, with the count when more than one item is (`+` from
ten), red when anything is overdue; a dim hollow dot while everything waits,
yellow when a condition it waits for cannot be read. A gate only a person
clears is the marker's slash already and adds no mark. Once the row's work
has landed its marker on the left carries the phase, so the mark is shown
only as the count of two or more things due. A folded section's
header counts markers only, so it never shows a mark its rows would not, and
its rollup in the details pane counts rows with something `due`, `owed,
waiting` and `overdue`. The Landed block has [its own pane](#the-landed-blocks-pane).

### Waiting rows

A watch that only tells you (`wt notify`, or `W` on the row) is not owed by
the row, so it is shown apart: the row wears a bell in the same badge slot,
after the owes mark: teal while it waits, yellow when a condition cannot be
read right now (a probe that fails, a pull request GitHub will not show),
since that watch may never fire, and green once everything it waited for held
and wt is telling you. In the details pane each is
one dim line after the owed list, its bell in the first column: `tells you when ○ merged`, `tells you at
17:00 (in 2h10m) · time to cut the release · by manager`, with `delivering`
or `held: automations paused` as above. The pane lists six and counts the
rest as `+N more (wt watch)`.

`W` opens **Notify me when `<slug>` is…** from the board on screen: `r`
ready, `c` checks green, `m` merged, `s` on staging, `p` in production.
Picking one sets a notification that is yours (`set by human` in `wt watch`);
picking one that is set cancels it, and your other notifications on the row
(set with `wt notify`) are listed below to cancel. A choice that cannot
happen stays listed, dimmed, with its reason: `(no PR)`, `(already merged)`,
`(same as merged here)` for staging without a
[deploy probe](configuration.md#environmentsstaging-and-environmentsproduction-optional-deploy-probes),
and the same for production with neither a probe nor a production branch.
Picking it only says why. The bell and the line appear at once and settle
when the host has stored the watch. The commit names the worktree the menu
showed by its creation time, so a menu left open while the worktree was
removed and created again writes nothing and reopens. Rows on a remote host
show what they wait for, but `W` is refused there: a notification raised on
another host cannot reach this desktop. A notification that outlived its
worktree (kept across a landed removal) has no row and is listed only by
`wt watch --all`.

Watches are delivered by the automation engine, so `Q`, `A` and `Ctrl+A`
work whenever a row waits or a fire is queued, even with no `[[automations]]`
configured. Pausing holds deliveries; conditions keep being evaluated.

### Row history

`H` opens the selected row's history as a modal, for an archived row and, in
the `h` view, a removed one too. Its header is the row's
marker, title and slug, then one summary line built from the events, such as
`created 3d ago · ready for 2h · 4 review rounds · 2 automations` (a landed
row leads with its phase instead of its asserted state: `due · merged 3m
ago`), showing
only the parts the events support. Below a rule the timeline reads newest
first under day headings. Each event is one line: a time, the glyph and
color the board uses for the same fact (a status wears its work-status dot, a
gated status the warn circle-slash, failing checks the red check glyph), a
headline in that color, dim metadata (who, PR number, rule, check names,
delivery state), then a status note or a quoted message in body gray. Long
lines are cut with an ellipsis, never wrapped.

A status line says only what is new. `by <who>` names whoever asserted it
unless that was the row's own agent, and is left out when no one was
recorded. A gate and an owed post-merge check are carried by every later
status, so `blocked on <gate>` and `check owed in <stage>` appear only on the
status where they first appear or change, compared with the status before
it; a status that drops the gate says `gate cleared` once. A gated status
keeps its warn circle-slash on every line.

Runs of noise fold into one line carrying the latest time: three or more
status changes back and forth between the same two states (`ready ↔ review
×4`), review rounds in a row from one reviewer (`review: 2 findings` with
the round count), and the same automation outcome repeated within ten
minutes (`×3`). Only neighbouring events fold, so a line never hides
something that happened in between. A status that sets or clears a gate or
an owed check is never folded.

A watch reads as at most two lines, both wearing the bell: when it was set,
and what wt did. Set (teal) says what it will do, with `at` for a time and
`when` for anything else: `will wake the session  at 23:08`, `will tell you
when merged`, `gate clears  when #3806 in production · then wakes the
session`, with `by <who>` unless that was the row's own agent or you. What wt
did (green) follows: `woke the session  at 23:08`, `told you  merged`,
`cleared the gate and woke the session  check flag=go held`. That one line
stands for everything recorded about the firing: the conditions holding, each
action taken within a minute of the one before, and the status change that is
wt clearing the gate, so no separate `gate cleared` status line appears. A
wake whose delivery nothing confirmed reads `wake sent, delivery not
confirmed` in yellow, and the attention feed says so once; it is not sent
again. `held  <what>` appears alone only while the conditions hold and nothing
has been done yet (automations paused, a busy session), and an action
delivered more than a minute after another keeps its own line. When the
conditions already held within ten seconds of the watch being set, its set
line folds in too and the line ends `as soon as it was set`. `stopped
waiting  for <what>` (dim) is a cancel, and `gave up waiting  for <what> ·
<why>` (yellow) is a watch wt dropped.

What the row owes reads as one line a change. `due: <what>` (yellow dot)
when a wake was delivered after the row's work landed, with `wt done <id>`
dim beside it; it stands in for that wake's `woke the session` line, and
`wake sent, delivery not confirmed` follows when nothing proved the delivery.
`done: <what>` (green) carries the evidence dim beside it, `again <when>:
<what>` (hollow dot) is an item put off with `--again`, and `dropped:
<what>` (dim) one that was let go. A check is named by its stage: `done:
staging check: <first line>`. `by <who>` follows unless that was the row's
own agent.
`wt history` says the same in sentences: `will wake the session at 23:08`,
`gate clears when <what>, then wakes the session (set by manager)`, `woke the
session: check flag=go held`, `told you: merged`, `cleared the gate: <what>`,
`held: <what>`, `stopped waiting for <what>`, `gave up waiting for <what>:
<why>`.

A skill or slash command used in the row's Claude Code session, when
`[history] skills` lists it, is one line with a terminal glyph: the command
in bright text, then its arguments and who ran it in dim, such as
`/fable-review` or `/check  src/foo · by agent`. Nothing follows a command
the person typed; `by agent` is the row's own agent invoking the skill, and
`by <who>` anyone else. The summary line counts them (`2 skill runs`). A
`wt mark` note wears a sticky-note glyph and reads `note  <text>`, with `by
<who>` unless the row's own agent or you added it. `wt history` says `ran
/fable-review`, `ran /check src/foo (by <slug>)`, and a note's text alone,
ending `(by <who>)` when an agent added it.

The modal is drawn from the board on every frame. While it is open, new
events appear at the top; a reader who has scrolled down stays on the line
they were reading. In a terminal too short to keep the header pinned, the
header scrolls with the timeline. A row with no events says that recording
starts from now. Times are local clock times under `Today`, `Yesterday`
and dated headings when the board carries prepared local labels for every
event shown; otherwise each line shows its age under elapsed-day headings
(`Past 24 hours`, `2 days ago`). The `removal record` pane of `h` shows the
same summary and timeline for a removed row that has recorded events.

## Keymap

| Key | Action |
|---|---|
| `j` / `k`, arrows | Move the selection; it stops at the first and last item |
| `g` / `G`, `Home` / `End` | First / last visible item |
| `PgUp` / `PgDn` | Move half of the list pane's height |
| `Space` | Next row needing you: saved work states plus failing checks, changes requested, unresolved threads, a merge wt armed that GitHub dropped from the queue (while the `pr` row says so), and failed actions; toasts when nothing needs you |
| `Tab` | Fold / unfold the section; unfolding lands on its first row and scrolls as many of its rows into view as fit under the cursor's three rows of context. Review requests fold too |
| `Ctrl+D` / `Ctrl+U` | Next section / the first row of this section, then the previous section. Review requests count as a section |
| `Ctrl+J` / `Ctrl+K` | Scroll details |
| `Ctrl+E` / `Ctrl+Y` | Scroll output |
| `r` / `Ctrl+R` | Refresh / ask for confirmation, then clear derived caches and refresh |
| `?` | Open searchable help; its title shows the running wt version and, with `[github.events]`, the events daemon's (`stale build` when it runs another build, so its snapshot is unused) |
| `Esc` | Return the bottom pane to its default feed |
| `q` / `Ctrl+C` | Quit |

### Worktrees and organization

The create prompt shows `New worktree name: ` before an empty editable field.
With remotes configured it shows `New worktree on <host> (Tab: host): `
instead. `Tab` cycles through this machine and each remote, and `n` starts on
the host chosen last in this session. A stacked create (`N`) opens on its
parent's host, and `Tab` does nothing once the text carries `--base`; a
`--base` typed after choosing a host resolves on that host.
A failed create keeps the name and chosen host editable, with the error shown
in red above the prompt.
Submitting selects a pending row immediately. Inventory and metadata replace
that same row once creation finishes; progress stays until then. Its group
opens while the pending row remains selected. Navigating elsewhere prevents
the later creation result from changing selection.

The Archived group sits at the bottom of the worktree pane when all visible
items fit. When the list is taller than the pane, it scrolls with the list.

Clean and delete use the host's existing GitHub data. They do not refetch PRs
when you press the key or confirm. A merged PR must match the current worktree
commit, or the checkout must hold only copies of the head it merged from
([cli.md](cli.md#wt-rm-slug)). Local file changes, unpushed work, and checkout identity are checked
again before removal. When landing evidence comes only from Git ancestry,
WT still checks the published base. Missing evidence does not mean merged.

| Key | Action |
|---|---|
| `n` / `N` | Create a worktree on the last chosen host (this machine by default) / create from the selected branch on its host |
| `Tab` (create prompt) | Cycle the new worktree's host |
| `Ctrl+N` | Create on a chosen host; toasts when `[remote]` is not configured. Managed hosts that owe a sign-in say so |
| `o` | Open the selected worktree in the configured editor |
| `d` | Remove the selected worktree after confirmation |
| `c` | Open cleanup review from background results, including archived rows. A landed row that still owes something is kept; rows whose work is simply unfinished are left out |
| `a` | Archive / restore the selected row; restore returns it to Inbox. A locked row is refused |
| `t` | Rename the worktree. A rename is final: no agent or tracker title replaces it, until `wt next` starts new work there |
| `#` | Set or clear the worktree's issue identity |
| `i` / `I` | Open the preferred issue / primary tracker issue |
| `s` | Open the deployed stage or dev URL when available |
| `z` | Open the row's change guide. Only with `[guides] enabled`; see [configuration.md](configuration.md#guides--change-guides) |
| `u` | Set or clear the work-status claim, note, or verification obligation (letters `t w r n h y v d` pick a state, `a` / `p` ready with a check owed in staging / production, `x` clears, `m` edits the note). A status picked here keeps the row's merge gate and its `--until` watch, as `wt status` does; only verified and dropped clear them. A pick that starts work a gate asserted on `todo` held back clears that gate and replies `Work status updated; start gate cleared (was: <gate>)`. Picking verified on a row that owes checks closes the due ones as `wt status verified` does; while one remains the reply says so (`✓ <slug> closed the check in staging: <first line>; still owes the check in production: the status stays ready until it is closed`) and the state is kept |
| `W` | Notify me when the row is ready (`r`), checks are green (`c`), it is merged (`m`), on staging (`s`) or in production (`p`); picking one that is set cancels it. See [Waiting rows](#waiting-rows) |
| `y` | Copy a worktree field: `b` branch, `s` stage name, `S` stage URL, `d` dev URL, `p` path, `n` slug, `i`/`I` issues, `r` PR. Unavailable entries stay listed. On a folded section, `n` copies the section name, `s` the slugs, `b` the branches, `l` a list |
| `l` / `L` | Move to a section (`l l` confirms, `l n` creates a new section; `Esc` returns to the picker) / rename the section. The picker lists every section in board order with its size |
| `D` | Delete the current section; its worktrees move to the Inbox (confirmation when it has any) |
| `J` / `K` | Reorder the selected row, stack, or folded or empty section. The move shows at the keypress; the cursor follows the row across sections, including into an empty one on screen, never into a hidden group. A press that can move nothing says why and sends nothing. When the stored order differs from the one on screen, nothing is written and the reply reads `Changed since you looked: …`. Rows in **Removing**, the archive and the Landed block do not reorder, and nothing moves into them. Under `[ui] sort = "status"` a row passes only neighbours of its own status, and a row that crosses a section edge enters first or last among the rows of its status |
| `b` | Record a fork base without rebasing: another worktree's branch (never the row or its descendants) or none; `b b` confirms |
| `R` | Restack or rebase the selected branch |
| `H` | Open the selected row's history: a timeline of how it got here. `j`/`k`, arrows, page keys and `g`/`G` scroll; `H`, `Esc` or `q` closes |
| `h` | Open removed-worktree history; `H` there opens the selected removed row's timeline |

### Worktree titles

wt never generates titles or summaries; a title is only ever set on purpose,
and the latest `wt status` note is the row's running summary. Each saved title
records its source, and a lower source never replaces a higher one:

| Source | Set by |
|---|---|
| `user` | `t`, or creating from the TUI. Final |
| `agent` | `wt new <words>`, `wt next --title`, or `wt title` from an agent. It may replace any title but a person's, as often as the outcome changes |
| `issue` | `wt new <ID>` with no title words: the tracker's title (`[issue_tracker] status_command`) |
| `fallback` | The same, when the tracker gave no title: the bare ID |

A creation from the TUI with a bare issue id does not wait for the tracker:
the row shows the id (`fallback`) and takes the tracker's title (`issue`) when
it arrives within a minute, unless the row was retitled meanwhile.

A worktree nobody titled (created from a branch or a one-word slug) shows its
PR title, then its first commit, then its slug. The details pane tags only a
person's title, `(user)`. Titles change only the display, never the branch or
directory.

Titles read in sentence case: `wt new fix calendar week` shows `Fix calendar
week`. Only a plain lowercase first word is capitalized, so `iOS` or `wt.toml`
stay as written, and a person's `t` rename is shown exactly as typed.
[`wt next`](cli.md#wt-next-branch---title-title---slug-slug---issue-idnone---gh-nnone)
starts new work, so its title replaces any earlier one, even a person's.

Creation selects the row after it appears in the prepared board. A row still
being created can already be filed with `l`: it shows in the chosen section at
once and moves there as soon as it exists. Everything else that needs a
checkout waits, including `J`/`K`. Remote
worktrees use the same host service for commands and sessions. On a
[managed remote](remote.md#managed-remotes), a request that needs a missing
sign-in (Codex for agents, GitHub for creation) first asks `Sign in on
<host>`; confirming runs the sign-in over SSH and then continues the request. Removal and
cleanup revalidate current hazards; unknown state does not authorize force
removal.

The `c` menu opens at once from the prepared background board. It does not wait
for Git, GitHub, or SSH. It also requests the same background refresh as `r`.
Candidates appear first under `Ready to remove` with green trash glyphs; kept
worktrees and their reasons follow under `Keeping` with their own count. Long
names and reasons wrap below their own column.

**Keeping lists only rows you could be surprised to see kept**: a row whose
own work landed, whose status is `dropped`, or whose PR merged or closed, and
that something still holds. The reasons are `still owes: <first line> (<when>)` (with
`(+N more)` when it owes more; a check, a pending wake or a due step, never
a gate alone), `uncommitted changes`, `rebase or merge is in progress`,
`operation in progress`, the unknowns (`checkout is unavailable`, `Git state
is unavailable`, `checkout changes are still loading`, `host is
unavailable`). A branch that will never land is still never removed by
cleanup, because its commits exist nowhere else, and its reason says how to
remove it deliberately: `dropped, but its commits are not on <base>: remove
it with d`, or `its PR was closed unmerged, and its commits are not on
<base>: remove it with d`. A row whose PR merged but whose landing wt cannot
prove reads `landing of its own work is not proven`. Work that is simply
unfinished or not started (not landed, not dropped, no merged or closed PR,
or a status of working or needs-human) is not cleanup's business and is left
out of the menu entirely. `wt clean` lists the same rows for the same reasons. With no candidates the overlay
stays open, starts with `Nothing is ready to clean.` and lists the kept rows,
if any.

While the menu is open the worktree list shows the same decision, so rows the
modal does not cover still read. Rows it will clean fill with green, and a
folded section holding one takes the same fill. A row the menu lists as kept
recedes to dim with only its title colored by why it stays: yellow when
something still holds the checkout (it still owes something, uncommitted
changes), the theme's info color (a muted purple, as review status uses) when
wt cannot tell yet (the host, checkout or Git state is unavailable, an
operation is in progress, changes are still loading), and
dim for `landing of its own work is not proven`. A row the menu leaves out wears no mark at
all and reads as it does on the board. The menu's reasons use the same colors.

Press `c` or `Esc` to close it. Reopen it to use newer
background results. Confirmation checks only the displayed candidate identities
against current landing evidence and removal hazards before queuing removal.

Remote worktree names have a server icon in salmon before the name. Archived
rows use the dim foreground color for this icon.

### Pull requests

| Key | Action |
|---|---|
| `p` | Open the selected pull request at the configured target (also `Enter` on a review request) |
| `g p` / `l p` | Open the PR in GitHub / Linear Reviews |
| `e` | Mark a draft PR ready after confirmation; toasts when it is already ready |
| `E` | Ship after confirmation, listing only the steps still needed; toasts when already shipped |
| `! m` | Toggle merge-when-ready |
| `f` | View failing check logs |
| `v` | Select requested reviewers |
| `i` on a review request | Open its linked issue |
| `w` | Check out a selected review request |
| `d` on a review request | Dismiss that review request |

Merge-when-ready uses the PR's base branch to choose merge queue or classic
auto-merge. `! m` arms or disarms based on what the row shows, so a quick
second press cancels even before GitHub reports the arm. Pending-check retries
retain the expected head SHA. A second `! m` cancels an idle retry; an
uncertain external result (a timeout or interrupted request) is shown as
unconfirmed and never resent; arming again is refused until a read shows the PR
armed or the unconfirmed attempt expires.

### Sessions and actions

| Key | Action |
|---|---|
| `F10` / `F11` / `F12` | Enter shell / diff / agent session |
| `Shift+F10` / `Shift+F11` | Stop shell / diff session after confirmation |
| `Shift+F12` | Start a new agent session: `c` Claude, `x` Codex, `o` OpenCode, `p` Pi, or `F12` for the highlighted one |
| `;` | Pick a session. Inside: `d` closes the highlighted live session gracefully and `x` kills it (a dead Claude row is forgotten), both without confirmation; `c`/`x`/`o`/`p` on a New row jump between harnesses; an empty Claude name picks one automatically |
| `Shift+Tab` | Cycle the primary harness |
| `!` | Open worktree actions: `m` merge when ready, `u`/`g` agent builtins, `d`/`s` dev server, `l` dev logs, `c` custom prompt, and configured actions |
| `m` / `M` | Enter the manager session / open manager commands (`d t o n a s` fleet plays, `r` ask about the row, `m` compact, `c` custom; `M M` confirms) |
| `,` / `.` / `/` | Enter the wt repo / main clone / dotfiles session when configured |
| `<` / `>` / `\` | Open the corresponding special-session palette (`g` continue, `m` compact, `z` open in editor, `c` custom) |
| `O` | Open the main clone in the editor |

F12 opens the existing live agent session even after Shift+Tab changes the
primary harness. It retains the exact session, including a named Claude session.
If several harnesses are live, F12 prefers the primary harness among them.
Use `;` to choose another session. If no session is live, F12 uses the primary
harness. A configured `harness.wt_session` still selects the `wt` special slot.

While session entry waits for a host, `Esc` cancels that connection attempt.
Navigation or another session request also cancels the wait. A cancelled remote
connection does not block entry to a local session.

When a matching remote build is unavailable, session entry stops without waiting
for the connection timeout. A “Cannot open remote session” dialog offers
Dismiss (Enter or Esc) and Copy details (`c`). Dismiss returns control to the
board. The copied details include the local build, remote platform, and release
lookup error, including GitHub 404 when that is the cause.

Every menu opens at the keypress, from the board on screen, for local and
remote rows: status `u`, notify `W`, fork base `b`, section move, sessions
`;`, start agent Shift+F12, stop a terminal Shift+F10/F11 (including one kept
after its process exited), reviewers `v`, GitHub `e`/`E`, review checkout
`w`, history restore, hard refresh, create (on this machine, a chosen host or
a remote row's host), removal `d`, cleanup `c`, and the action palettes (`!`,
`M`, `<` `>` `\`). An open menu updates as newer data
arrives and keeps the highlighted choice. If the highlighted choice
disappears, the cursor stays in place, a notice names it, and the next key is
ignored unless it moves the cursor or cancels. If what the menu showed changed
before the host applied the choice (a PR moved, a session was replaced, an
agent set a status), nothing is written: the menu reopens with the current
data and the toast says what changed. An open confirmation never changes
what it will do: when its subject changes on the board (another run in the
slot, another pull request or checkout), it stays as shown and the host
refuses it. A "Stop …?" confirmation whose run ends while it is open reads
`<name> has finished`, its hint reads `⏎ / esc close`, and any answer key
only closes it.

A key whose menu the board cannot build answers at once in the footer
instead of opening one. `e`, `E`, `v` or `f` on a row with no shown pull
request reads `GitHub data is unavailable: <error>` when the row's GitHub
read failed, `no PR for this branch` when the read covered the branch, and
`GitHub data is still loading for this branch` otherwise. With
`WT_GITHUB=off` these keys, and every pull-request entry of a palette, read
`GitHub is off (WT_GITHUB=off)` and nothing is asked or waited for. `v` reads
`reviewer list is still loading` until the reviewer list has been read once;
it is read again about once a day and on a hard refresh. A remote row reads
`session list not loaded yet` before its host's lists arrive, and `session
list unknown: <error>` while that host is disconnected. An answer about
data that is unavailable or still loading also asks the sources to read
again. `no PR for this branch` asks its host to read that one branch from
GitHub: a pull request opened since the last read is absent only until the
next one.

Two keys fetch content only a host has: failed checks `f` and dev logs
`! l`. The overlay opens at the keypress reading `loading…` (`loading… 7s`
once three seconds have passed), its hint line names the keys that close it
(`esc / q / f`), and the reply fills it. While it loads, every key belongs to the overlay: Esc, `q`, Ctrl+C
or the overlay's own key (`f`, `l`) closes it and cancels the fetch, and
nothing else typed is kept or replayed. `f` answers at once, with no fetch,
when the board shows no failing check (`no failing checks`), no pull
request, or one that is not open.

In an action palette, choosing an unavailable entry says why in the footer
and keeps the palette open. An entry that needs a pull request, chosen before
GitHub has confirmed the row's branch, waits in the open palette marked
`waiting for GitHub… Esc cancels`, for up to 12 seconds, and the footer reads
`<name>: waiting for GitHub to confirm <branch>… Esc cancels`. Closing the palette
or any later key that does something ends the wait. When GitHub confirms, a
command runs; an entry whose next step is a prompt stays highlighted and the
footer reads `<name>: GitHub confirmed; Enter continues`, so no prompt opens
without a key. An entry for a pull request the board does not show yet reads
`<name>: the board does not show its pull request yet`. Recent values in an
argument picker for a remote row arrive with that host's live data; before
that the prompt is a plain text field.

A key pressed while 64 requests already wait for the host is refused with
`actions are busy; try again when one finishes`, and shows no change. A
follow-up wt queued for itself waits for room, and is dropped with `a queued
action was not sent: actions were busy` once another key has acted.
Host replies are still applied while a session holds the terminal, so a
worktree created just before entering a session starts its checkout without
waiting for you to come back.

The session picker runs on the selected host. A running Codex, OpenCode, or Pi slot
whose conversation UUID cannot be proven appears as **current session
(conversation not identified)**. Enter reconnects to that exact terminal;
historical conversations remain separate. Live selections capture the tmux
session ID and creation time. If the session changes or ends, attachment and
stop actions refuse the stale selection and ask you to reopen the picker.

While attached to a worktree session, F10 opens its shell, F11 opens its diff,
and F12 returns to its agent. Press the key for the current session to return
to the board. Switching between sessions does not require a board visit.
Special slots such as the manager retain their board-return behavior. `/compact`
is available in harness command palettes that support it. Manager and special
session controls remain reachable by these keys. The footer shows live
special-session state and manager context usage when those facts are available.
If a terminal command fails, its output remains visible instead of ejecting you
to the board. Press Enter to retry that command, or press the current session's
function key to return to wt. Commands
that exit successfully close normally.

### Automations and output

| Key | Action |
|---|---|
| `Q` | Automations overlay: running, queued (with when each runs) and the last ten recorded fires, watch deliveries included; `A` pauses inside it. Toasts when no `[[automations]]` is configured and nothing waits |
| `A` | Pause / resume all automations |
| `Ctrl+A` | Pause / resume automations for the selected worktree |
| `Ctrl+Shift+A` | Cancel queued automations |
| `'` | Pick an activity, session, or action output |
| `[` / `]` | Previous / next output |
| `"` | Toggle attention / all activity |
| `x` / `X` | Mark attention as seen |

### Performance and errors

Press `P` for the process and system performance view. `r` takes a sample,
`c` toggles repeated sampling, `i` sends the snapshot to the wt repo session
and enters it, `j`/`k`, page keys, and `Ctrl+D`/`U`/`E`/`Y` scroll, and `Esc`,
`q`, `P`, or `Ctrl+C` closes it. The same snapshot is available with [`wt perf`](cli.md#wt-perf---json).
Command and source failures remain visible in the footer, source state, or
activity/attention feed. The native TUI does not provide a full-screen
uncaught-error recovery overlay.

## Picker and text input

Help, the performance view, and log overlays (such as dev-server logs) share
one scroll keymap: `j`/`k`, arrows, and `Ctrl+E`/`Ctrl+Y` move three lines;
`PgUp`/`PgDn` and `Ctrl+U`/`Ctrl+D` move half a page; `g`/`G` or `Home`/`End`
jump to the edges. `Esc`, `q`, and `Ctrl+C` close them, and so does the
overlay's own key: `?` for help, `P` for the performance view, `f` for failed
checks, `l` for dev logs. Each overlay's hint line names its keys.

Use `j`/`k`, arrows, page keys, and `g`/`G` to move through lists. `Enter`
confirms; `Esc`, `q`, and `Ctrl+C` cancel, and confirmations also cancel with
`n` or their opening key. `Space` never confirms. Repeating the opening key
confirms the current selection. Digits `1`-`9` pick list entries, such as sessions, saved values,
statuses, sections, bases, outputs, and harnesses, but not palette rows,
which use their letters. Reviewer selection
uses `Space` to toggle entries. `Esc` or `Backspace` on an empty sub-prompt,
such as a new section or session name, returns to its picker. Text fields support cursor movement, word movement,
Unicode-safe deletion, and `Ctrl+U`/`Ctrl+K` to clear to the start/end. A
custom `! c` prompt keeps pasted line breaks.

## Mouse

The wheel scrolls the pane under the pointer. Over the list it scrolls the
viewport without moving the cursor; the next cursor move, or any key that acts
on the selection or opens a menu, scrolls back to it.
Dragging selects text within one pane and copies it to the clipboard on
release.

## Deliberate behaviors

Several behaviors are intentional rather than incidental:

- The dirty badge stays on landed rows. Cleanup would discard those files,
  so the hazard stays visible until the row is removed.
- Folded section headers show a count per work state beside the row count,
  so a folded group still says what needs attention.
- Landed rows take their title from the landing commit when no title was
  set, rather than repeating the branch name.
- Modals use rounded borders and pane titles are dim, so the focused modal
  stands apart from the panes behind it.
- The perf overlay leads with a verdict on whether wt is the load, then
  meters, a per-category breakdown, and the heaviest processes inside and
  outside wt's process tree.
- Help adds selection, sync notation, and create-prompt flag sections, and
  its search finds keys by their aliases (`/rebase` finds `R` and `b`).
- The create prompt reads `New worktree name:` rather than `new:`, so an
  empty field says what it expects.
- A key with nothing to act on, such as `e` without a PR, shows a yellow
  toast and is not recorded as a failed action on the attention feed.
- Remove confirmation revalidates hazards before removal, and `f` keeps its
  log overlay.
- `Space` jumps through a broader attention set (status, checks, review,
  failed action) and toasts when nothing needs you.
- `Esc` output focus is global rather than per worktree. Errors appear on
  the attention feed rather than in a dedicated error overlay, and `F12` on
  a remote row does not queue an attach.
- The work-status header colors `risk low` / `risk high` by the risk, as the
  list's risk column does, rather than by the state, so a high-risk `ready`
  never reads green.
- A landed row's git line names the base as the branch line does
  (`merged into main`) rather than always saying `origin/main`.
- The git line leaves out a sync group it has no counts for (a branch with
  no upstream, a base not read yet) instead of drawing an empty pair of brackets.

Session `; d` and `; x` act immediately, without confirmation; the
review-requests fold is not persisted.

The session picker is built from the board: saved conversations from the
background history snapshot and the live tmux slots. Opening it sends a
background read, so saved conversations join the open picker when discovery
finishes, without a loading row. With an empty cache it shows live slots and
new-session choices. A Claude primary that has never run (no transcript and
no live session) is not listed, so the picker offers New. A session whose
process is gone shows no running state. Attachment rechecks the selected live identity; a
new session chosen beside a live slot the picker did not show is refused and
the picker reopens. A picker cursor that has not moved follows the newest
default, so a picker opened before a live slot appeared selects it once it
does. Shift+F10 and Shift+F11 confirm from the board for local and remote
rows, including a terminal kept after its process exited (`Its process has
exited; closing it discards the output kept for inspection.`). The stop
names the terminal the board showed by its tmux id and creation time; a
terminal replaced since is not stopped, and the confirmation reopens.

The `d` delete menu opens from cached worktree data, including dirty and remote
rows. It shows current cached warnings. After confirmation, the host checks
identity and removal hazards. Any hazards or stage destruction require a fresh
confirmation before removal. That confirmation waits until any open menu,
overlay or prompt closes. A missing HEAD shows an information menu only.
