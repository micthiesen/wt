# TUI guide

Run `wt` with no arguments to open the terminal interface. Press `?` for the
live keymap; use `/` to filter help. The help text is rendered by the native
application and is the most current reference.

## Layout

At 80 columns or wider, the list uses about 42% of the width and the right
column contains details above the activity feed. Narrower terminals stack the
list above the right column. With `[ui].activity_pane = "full_width"`, list and
details share a top region capped at 22 rows and the activity feed spans the
bottom. Terminals smaller than 20 columns or 5 rows show a resize prompt.

The list shows a stack tree prefix, a work-status marker, the worktree title,
issue status when available, and PR/session/automation badges. Work-status
markers use color for state: red for needs-human, yellow for needs-testing or
working, green for ready or verified, cyan for review, dim hollow for todo or
unset, and dim slash for dropped. A hollow marker in a state's color means the saved
status SHA differs from the current observed HEAD. A gated ready/todo status
uses a yellow slash, and overdue post-merge verification is red. The list does
not change the marker into a Git merge or commit glyph when a branch lands;
landing, rebase, and conflict facts appear in the Git details group.

Sections can be folded with `Tab`. A folded header summarizes work states,
risk, stale or overdue status, Git dirtiness and upstream movement, rebases or
conflicts, PRs and checks, paused automations, attention, and up to two blocker
notes. Select the section to see the full rollup and blocker notes above its
member rows. The summary is prepared from the same typed facts used by the
detail pane. `[ui].sort = "status"` orders rows by work status; manual section
placement remains separate.

The details pane begins with the title, branch, and path, followed by the work
status, risk, status age, note, gate, and verification obligation when present.
`[ui].rows` controls the order and visibility of typed detail groups: `branch`,
`issue` (also `linear`), `stage`, `dev`, `pr`, `claude` (the cross-harness AI
session group), and `git`. The optional `path` group can be added explicitly.
Unknown group names are skipped. A GitHub or host error remains visible as an
error, and unavailable facts remain unknown rather than being shown as clean.

Title precedence is saved manual title, generated title, PR title, first commit
title, then slug. The pane does not show a title-source tag. The AI session
group lists live or discovered Claude, Codex, and OpenCode sessions. When none
is listed, it shows the configured primary harness and the F12 start hint.
Selected session output starts with its available transcript summary.

The activity pane shows attention and activity feeds, session output, and
tracked action output. Attention is the curated feed; activity contains the
broader event stream. Entries show local time, wrap to the pane width, and
scroll by visual rows. The attention watermark is saved and `x` marks the
current feed as seen. `Ctrl+J`/`Ctrl+K` scroll details; `Ctrl+E`/`Ctrl+Y`,
`Ctrl+Shift+J`/`Ctrl+Shift+K` where supported, and the mouse wheel scroll the
output pane. Selecting an output with `'` pins it until it ends; `[`/`]` cycle
outputs, `"` toggles attention/all activity, and `Esc` returns to the default.
The feed is restored from the app log on startup.

The footer shows the active prompt, a toast, a source error, or a static key
hint. When the normal key hint is shown and facts are available, it prefixes
live manager/main/wt/dotfiles session states; the manager Claude session also
shows context occupancy after a transcript turn provides usage. These are
status labels, not clickable session buttons.
Remote session entry is available after its worktree is present; F12 does not
queue an automatic attach for a worktree that is still being created.

When `h` opens removed-worktree history, rows are grouped by local day starting
at 04:00. `↑` marks proved production landing, `✓` marks proved base landing,
and age appears beside the row. Older records can be checked against a bounded
local GitHub-merge and production-history scan while history is open. Select a
row to see its removal time, host, saved status, issue/PR facts, and restore
details. `Enter` restores, `p` opens the PR, `i` opens the issue when known, and
`y` copies the branch.

## Keymap

| Key | Action |
|---|---|
| `j` / `k`, arrows | Move the selection |
| `g` / `G` | First / last visible item |
| `Space` | Move to the next row needing attention |
| `Tab` | Fold / expand the section |
| `Ctrl+D` / `Ctrl+U` | Next / previous section |
| `Ctrl+J` / `Ctrl+K` | Scroll details |
| `Ctrl+E` / `Ctrl+Y` | Scroll output |
| `r` / `Ctrl+R` | Refresh / clear derived caches and refresh |
| `?` | Open searchable help |
| `q` / `Ctrl+C` | Quit |

### Worktrees and organization

The Archived group sits at the bottom of the worktree pane when all visible
items fit. When the list is taller than the pane, it scrolls with the list.

Clean and delete use the host's existing GitHub data. They do not refetch PRs
when you press the key or confirm. A merged PR must match the current worktree
commit. Local file changes, unpushed work, and checkout identity are checked
again before removal. When landing evidence comes only from Git ancestry,
WT still checks the published base. Missing evidence does not mean merged.

| Key | Action |
|---|---|
| `n` / `N` | Create a worktree / create from the selected branch |
| `Ctrl+N` | Start the create flow; configured remotes add a host chooser |
| `o` | Open the selected worktree in the configured editor |
| `d` | Remove the selected worktree after confirmation |
| `c` | Review and clean eligible merged or gone worktrees |
| `a` | Archive / restore the selected row |
| `t` / `T` | Edit title / regenerate its AI title |
| `#` | Set or clear the worktree's issue identity |
| `i` / `I` | Open the preferred issue / primary tracker issue |
| `s` | Open the deployed stage or dev URL when available |
| `V` | Show or hide post-merge verification steps |
| `u` | Set or clear the work-status claim, note, risk, or verification obligation |
| `y` | Copy a selected worktree field |
| `l` / `L` | Move to a section / rename the section |
| `J` / `K` | Reorder the selected row or group |
| `b` | Record a fork base without rebasing |
| `R` | Restack or rebase the selected branch |
| `h` | Open removed-worktree history |

Creation selects the row after it appears in the prepared board. Remote
worktrees use the same host service for commands and sessions. Removal and
cleanup revalidate current hazards; unknown state does not authorize force
removal.

### Pull requests

| Key | Action |
|---|---|
| `p` | Open the selected pull request |
| `g p` / `l p` | Open the PR in GitHub / Linear Reviews |
| `e` | Mark a draft PR ready after confirmation |
| `E` | Run the configured ship flow after confirmation |
| `! m` | Toggle merge-when-ready |
| `f` | View failing check logs |
| `v` | Select requested reviewers |
| `w` | Check out a selected review request |
| `d` on a review request | Dismiss that review request |

Merge-when-ready uses the PR's base branch to choose merge queue or classic
auto-merge. Pending-check retries retain the expected head SHA. A second `! m`
cancels an idle retry; an uncertain external result is not retried blindly.

### Sessions and actions

| Key | Action |
|---|---|
| `F10` / `F11` / `F12` | Enter shell / diff / agent session |
| `Shift+F10` / `Shift+F11` | Stop shell / diff session after confirmation |
| `Shift+F12` | Choose a harness for a new agent session |
| `;` | Pick or manage named sessions |
| `Shift+Tab` | Cycle the primary harness |
| `!` | Open worktree actions and configured actions |
| `m` / `M` | Enter the manager session / open manager commands |
| `,` / `.` / `/` | Enter the wt repo / main clone / dotfiles session when configured |
| `<` / `>` / `\` | Open the corresponding special-session command palette |
| `O` | Open the main clone in the editor |

While attached, the configured terminal handoff returns to the TUI. `/compact`
is available in harness command palettes that support it. Manager and special
session controls remain reachable by these keys. The footer shows live
special-session state and manager context usage when those facts are available.

### Automations and output

| Key | Action |
|---|---|
| `A` | Pause / resume all automations |
| `Ctrl+A` | Pause / resume automations for the selected worktree |
| `Ctrl+Shift+A` | Cancel queued automations |
| `'` | Pick an activity, session, or action output |
| `[` / `]` | Previous / next output |
| `"` | Toggle attention / all activity |
| `x` | Mark attention as seen |

### Performance and errors

Press `P` for the process and system performance view. `r` takes a sample,
`i` enables repeated sampling, `j`/`k` and page keys scroll, and `Esc` or `q`
closes it. The same snapshot is available with [`wt perf`](cli.md#wt-perf---json).
Command and source failures remain visible in the footer, source state, or
activity/attention feed. The native TUI does not provide a full-screen
uncaught-error recovery overlay.

## Picker and text input

Use `j`/`k`, arrows, page keys, and `g`/`G` to move through lists. `Enter`
confirms; `Esc`, `q`, and `Ctrl+C` cancel. Repeating the opening key confirms
the current selection in pickers that show that chord. Reviewer selection uses
`Space` to toggle entries. Text fields support cursor movement, word movement,
Unicode-safe deletion, and `Ctrl+U`/`Ctrl+K` to clear to the start/end.
