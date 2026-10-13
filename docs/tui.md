# TUI guide

Run `wt` with no arguments to open the terminal interface. Press `?` for the
live keymap; use `/` to filter help. The help text is rendered by the native
application and is the most current reference.

## Layout

The list takes 44% of the width, clamped to 32..52 columns, and the right
column holds details above the activity feed. Details get at most 20 rows; the
feed gets the rest and at least 7. Below 60 columns the panes stack vertically.
With `[ui].activity_pane = "full_width"`, list and details share the top region
and the activity feed spans the bottom. Terminals smaller than 20 columns or 5
rows show a resize prompt. The UI uses a Nord-derived palette and Nerd Font
glyphs; a terminal font without Nerd Font symbols shows placeholder boxes.

Each section starts with a dim rule naming it, separated from the previous
section by a blank line. A row shows, left to right: the stack rail (tinted per
stack lane), the work-status marker, the title (prefixed with the issue number
when the issue is tracked), a dim `→ Section` when a stack member sits apart
from its parent's section, and a right-aligned glyph cluster. The cluster's
fixed order is action running, dirty, rebase or conflict, deploy, AI session
state, review bot, review decision, PR state or merge-queue position, and CI
checks. `[ui].hidden_badges` removes slots by name (`action`, `dirty`,
`rebase`, `deploy`, `session`, `review_bot`, `review`, `pr`, `checks`).
Archived rows render dimmed. Review requests and the archive sit at the bottom
of the list. The cursor keeps three rows of context while scrolling and the
list border carries a scrollbar when rows overflow.

Work-status markers use color for state: red for needs-human, yellow for
needs-testing or blocked, cyan for working, purple for review, green for ready
or verified, and a dim hollow dot for todo or unset. A slash marks a gated or
dropped status. A hollow marker in a state's color means the saved status SHA
differs from the current observed HEAD, and overdue post-merge verification is
red. A landed branch without a newer status shows the green merge glyph.

Sections fold with `Tab`. Only folded sections are cursor stops; an expanded
section is just its rows. A folded header reads `[×NN] Name` with a
right-aligned count per work state, most urgent first (ready, needs-human,
needs-testing, review, working), plus overdue verifications, conflicts, and
failing checks. Selecting it shows the full rollup and blocker notes in the
details pane. `[ui].sort = "status"` orders rows by work status; manual section
placement remains separate. Within a section, rows without a manual order sort
first, and a stack sorts as one unit under its root's identity.

The details pane's border names the slug. It starts with the wrapped title
and a dim source tag: `manual`, `llm`, `pr`, `commit`, or `slug`, following the
title precedence of saved manual title, generated title, PR title, first commit
title, then slug. The work-status block follows when a status is saved: state,
risk, age, and commits since the status, then the gate and the note behind a
rail in the state's color. `OPS`, `REVERT`, `IF WRONG`, and `UNTESTED` note
fields align under a hanging indent. A verify-after-merge obligation shows its
step count with a two-line preview; `V` expands or collapses the steps, and
they start expanded when the check is due.

Labeled rows follow, with dim right-aligned labels. `[ui].rows` controls their
order and visibility: `branch` (with its landing base), `issue` (also
`linear`), `stage`, `dev`, `pr` (number, merge queue or auto-merge, checks with
failing names, review, review bot), `claude` (every Claude, Codex, and OpenCode
session, the F12 target first), and `git` (dirty or clean, diff against the
merge-base, last commit and creation ages, upstream counts in parentheses, base
counts in brackets). The optional `path` group can be added explicitly and
keeps the checkout name visible when truncated. Unknown group names are
skipped. On narrow panes the `pr` and `git` rows drop their least important
parts first. A GitHub or host error remains visible as an error, and an
unavailable fact shows `—` rather than reading as clean.

Below the rows come a rebase block when the branch would not rebase cleanly
onto its base (listing up to eight conflicting files from a `git merge-tree`
pre-flight), paused automations, the session summary, PR comments, and the
unresolved thread count. A folded section's details show its rollup, member
rows, a red "blocked on you" group with each needs-human member's note, and a
yellow "blocked on" group with external gates. A review request's details show its state, branch,
author, checks, and keys. The details pane scrolls with `Ctrl+J`/`Ctrl+K` and
shows a scrollbar when it overflows.

Creation shows progress in the header and footer, separate from worktree rows.
F12 queues agent entry on the chosen host once the checkout is ready. Esc or
list navigation returns focus to existing worktrees. Attention and other board
controls remain available. Inventory is the only source of worktree rows, even
when it arrives before the creation reply. Dependency installation continues
as a background action. Confirmed removal moves the row to Archived with a
removal glyph; a failed removal restores its previous archive state.

### Cached, pending and failed states

At launch every pane shows the last data wt saved for it, so the board is
populated before the first fetch. Each pane's title names its state the same
way: `cached 3m` while it shows saved data (from this or a remote host),
`loading…` before any data exists, `↻` while a fetch takes a moment, and
`<source> failed` when a fetch failed (the last good data stays visible; saved
data that also failed to refresh reads `cached 3m · GitHub failed`). The list
and header summarize every host; a row's details reflect only that row's host.

Every change you make appears at once. Worktree rows contain their title,
issue identifier, stack references and status glyphs only, never status text.
One fixed-width pending glyph marks `…` while a request is in flight, `↻`
once accepted, or `?` when its outcome is unknown. Uncertainty takes precedence
when several changes are pending. Full explanations, such as
`? arming merge (unconfirmed)`, stay in the details title. Unknown action
outcomes and expired updates also enter the attention feed. Board-wide changes,
such as switching the primary harness, show in the header. A failed request
rolls back at once with an error toast. A change that a fresh read still does
not show after its deadline (30 seconds for GitHub, reviews and issue statuses;
12 seconds for local state; 10 minutes for creating or removing a worktree) is
dropped with a notice; nothing is resent automatically. A key that turns out to
have nothing to do (for example, moving a row that is already at the top) ends
its marker at once. Toggles act on what the row shows, and GitHub writes are refused
if the PR or its head commit changed since you saw it.

The bottom pane shows the attention feed, all activity, session output, or
tracked action output; its title names which, such as `attention · 2 new` or
`slug · claude / name · live · 1/3`. Rows show a dim local time, the source,
and the message. Attention entries wrap with a hanging indent; the full feed is
one line per event, colored by level. Entries already marked seen are dimmed
below a `seen HH:MM:SS` rule, and an empty attention feed reads "nothing needs
you". The attention watermark is saved and `x` or `X` marks the current feed as seen.
`Ctrl+E`/`Ctrl+Y`, `Ctrl+Shift+J`/`Ctrl+Shift+K` where supported, and the
mouse wheel scroll the output pane. Selecting an output with `'` pins it until
it ends; `[`/`]` cycle outputs, `"` toggles attention/all activity, and `Esc`
returns to the default. The feed is restored from the app log on startup.

The header shows worktree and archive counts, refresh state, paused or queued
automations, the primary harness's rate-limit windows (colored as they near
the limit, with time to reset), and the primary harness glyph. The footer shows
the active prompt, a toast (failures in red with a glyph), a source error, or a
static key hint, and on the right the manager, main, wt, and dotfiles slot
sessions as `[m] [.] [,] [/]` colored by session state. The manager Claude
session also shows context occupancy after a transcript turn provides usage.
These are status labels, not clickable buttons.

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
is open. The `removal record` pane shows removal time, host, saved status,
verification, landing, and notes. `Enter` restores, `p` opens the PR, `i` opens
the issue when known, and `y` copies the branch. `p` and `i` toast when nothing is recorded, and `O`
opens the main clone as it does on the board.

## Keymap

| Key | Action |
|---|---|
| `j` / `k`, arrows | Move the selection; at the first or last item, scroll the list to that edge |
| `g` / `G`, `Home` / `End` | First / last visible item |
| `PgUp` / `PgDn` | Move half a page |
| `Space` | Next row needing you: saved work states plus failing checks, changes requested, unresolved threads, and failed actions; toasts when nothing needs you |
| `Tab` | Fold / unfold the section; unfolding lands on its first row. Requested reviews fold too |
| `Ctrl+D` / `Ctrl+U` | Next / previous section |
| `Ctrl+J` / `Ctrl+K` | Scroll details |
| `Ctrl+E` / `Ctrl+Y` | Scroll output |
| `r` / `Ctrl+R` | Refresh / clear derived caches and refresh |
| `?` | Open searchable help |
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
After creation, WT refreshes inventory and metadata and selects the new row
once it appears; progress stays until then. Its group opens when the row
arrives, including if a folded snapshot arrives before the saved group state.

The Archived group sits at the bottom of the worktree pane when all visible
items fit. When the list is taller than the pane, it scrolls with the list.

Clean and delete use the host's existing GitHub data. They do not refetch PRs
when you press the key or confirm. A merged PR must match the current worktree
commit. Local file changes, unpushed work, and checkout identity are checked
again before removal. When landing evidence comes only from Git ancestry,
WT still checks the published base. Missing evidence does not mean merged.

| Key | Action |
|---|---|
| `n` / `N` | Create a worktree on the last chosen host (this machine by default) / create from the selected branch on its host |
| `Tab` (create prompt) | Cycle the new worktree's host |
| `Ctrl+N` | Create on a chosen host; toasts when `[remote]` is not configured. Managed hosts that owe a sign-in say so |
| `o` | Open the selected worktree in the configured editor |
| `d` | Remove the selected worktree after confirmation |
| `c` | Open cleanup review from background results, including archived rows |
| `a` | Archive / restore the selected row; restore returns it to Inbox. A locked row is refused |
| `t` / `T` | Edit title / regenerate its AI title |
| `#` | Set or clear the worktree's issue identity |
| `i` / `I` | Open the preferred issue / primary tracker issue |
| `s` | Open the deployed stage or dev URL when available |
| `V` | Show or hide this row's post-merge verification steps (open by default when the check is due) |
| `u` | Set or clear the work-status claim, note, risk, or verification obligation |
| `y` | Copy a worktree field: `b` branch, `s` stage name, `S` stage URL, `d` dev URL, `p` path, `n` slug, `i`/`I` issues, `r` PR. Unavailable entries stay listed |
| `l` / `L` | Move to a section / rename the section |
| `J` / `K` | Reorder the selected row, stack, or folded group; the cursor follows the row across sections |
| `b` | Record a fork base without rebasing: another worktree's branch (never the row or its descendants) or none |
| `R` | Restack or rebase the selected branch |
| `h` | Open removed-worktree history |

Creation selects the row after it appears in the prepared board. Remote
worktrees use the same host service for commands and sessions. On a
[managed remote](remote.md#managed-remotes), a request that needs a missing
sign-in (Codex for agents, GitHub for creation) first asks `Sign in on
<host>`; confirming runs the sign-in over SSH and then continues the request. Removal and
cleanup revalidate current hazards; unknown state does not authorize force
removal.

The `c` menu opens at once from the prepared background board. It does not wait
for Git, GitHub, or SSH. It also requests the same background refresh as `r`.
With no candidates, the overlay stays open and shows
why each row is kept. Candidates appear first with green trash glyphs; kept
worktrees and their reasons are muted under a separate count. Long names and
reasons wrap below their own column, including in the no-candidates view.
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
| `Shift+F12` | Start a new agent session: `c` Claude, `x` Codex, `o` OpenCode, or `F12` for the highlighted one |
| `;` | Pick a session. Inside: `d` closes the highlighted live session gracefully and `x` kills it (a dead Claude row is forgotten), both without confirmation; `c`/`x`/`o` on a New row jump between harnesses; an empty Claude name picks one automatically |
| `Shift+Tab` | Cycle the primary harness |
| `!` | Open worktree actions: `m` merge when ready, `u`/`g` agent builtins, `d`/`s` dev server, `l` dev logs, `t` rename with AI, `c` custom prompt, and configured actions |
| `m` / `M` | Enter the manager session / open manager commands |
| `,` / `.` / `/` | Enter the wt repo / main clone / dotfiles session when configured |
| `<` / `>` / `\` | Open the corresponding special-session palette (`g` continue, `m` compact, `z` open in editor, `c` custom) |
| `O` | Open the main clone in the editor |

The session picker runs on the selected host. A running Codex or OpenCode slot
whose conversation UUID cannot be proven appears as **current session
(conversation not identified)**. Enter reconnects to that exact terminal;
historical conversations remain separate. Live selections capture the tmux
session ID and creation time. If the session changes or ends, attachment and
stop actions refuse the stale selection and ask you to reopen the picker.

While attached, the configured terminal handoff returns to the TUI. `/compact`
is available in harness command palettes that support it. Manager and special
session controls remain reachable by these keys. The footer shows live
special-session state and manager context usage when those facts are available.
If a terminal command fails, its output remains visible instead of ejecting you
to the board. Press Enter to retry that command, or F12 to return to wt. Commands
that exit successfully close normally.

### Automations and output

| Key | Action |
|---|---|
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
and enters it, `j`/`k`, page keys, and `Ctrl+D`/`U`/`E`/`Y` scroll, and `Esc` or
`q` closes it. The same snapshot is available with [`wt perf`](cli.md#wt-perf---json).
Command and source failures remain visible in the footer, source state, or
activity/attention feed. The native TUI does not provide a full-screen
uncaught-error recovery overlay.

## Picker and text input

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
viewport without moving the cursor; the next cursor move scrolls back to it.
Dragging selects text within one pane and copies it to the clipboard on
release.

## Differences from the TypeScript UI

The native TUI keeps the TypeScript board's keys, chords, prompts, glyphs,
and spacing. It departs from that UI only where the change is deliberate:

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

Session `; d` and `; x` act immediately, as they did in TypeScript; the
review-requests fold is not persisted, also as before.
