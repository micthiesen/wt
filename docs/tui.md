# TUI guide

`wt` with no arguments launches the TUI. Press `?` inside for the built-in keymap + glyph legend (with `/` to filter it) — that overlay is always the most current reference; this page is the tour.

## Layout

- **List pane** (left): one line per worktree — slug, status glyphs, PR/CI badges, session indicators — grouped into sections, with stacks rendered as trees. A pinned "review requests" section surfaces PRs waiting on your review.
- **Details pane** (right): the configured rows (`[ui].rows` in [configuration.md](configuration.md#ui)) for the selected worktree — branch, base, Linear issue, stage, PR, sessions, git state — then a rebase-state block (restacking / mid-rebase / conflict with the clashing files) when something is moving, plus the AI-generated title/description band when `[ai]` is configured.
- **Bottom pane**: live outputs — harness sessions, action runs, event feeds. Auto-follows the selected row; `'` picks an output explicitly, `[` / `]` cycle, `Esc` returns to auto-follow.
- **Footer**: key legend, or a text prompt when one is active (`n` local new-worktree, `Ctrl+N` remote new-worktree, `L` rename section).

Freshness is push-based: fs watchers on git refs, worktree dirs, locks, and the state files — plus the optional [GitHub webhook daemon](github-events.md) — invalidate exactly what changed. `r` re-fetches as a backstop; `Ctrl+R` (with confirm) nukes all cached data and refetches from scratch.

## Keymap

### Navigation

| key | action |
|---|---|
| `j`/`k`, arrows | move cursor |
| `g` / `G` | jump to top / bottom |
| `Tab` | fold/unfold the section under the cursor |
| `Ctrl+J` / `Ctrl+K` | scroll the details pane |
| `h` | flip to the removed-worktrees history view |

### Worktree actions

| key | action |
|---|---|
| `n` / `N` | new local worktree prompt (accepts a Linear URL/id, branch, or slug, plus `--any`, `--base <ref>`); `N` pre-fills `--base` with the selected row's branch |
| `Ctrl+N` | create on `[remote]`; the worktree stays in this Inbox with a remote glyph, and F10/F11/F12 route that row's sessions over SSH |
| `o` | open the worktree in Zed |
| `d` | remove locally or on the row's remote host (confirm; escalates to a force-remove warning when dirty/unpushed) |
| `c` | clean all merged/gone worktrees (confirm) |
| `a` | archive / restore the row |
| `i` | open the Linear issue |
| `s` | open the deployed stage URL |
| `t` | regenerate the AI summary |
| `y` | yank menu — copy branch (`b`), stage (`s`), stage URL (`S`), path (`p`), slug (`n`), issue URL (`i`), PR URL (`r`) |
| `r` / `Ctrl+R` | refresh / hard refresh (clear caches, confirm) |

When the SSH host is sleeping or offline, its last-known worktrees remain in
the Inbox with `host unavailable`. The title bar also shows an offline warning;
F10/F11/F12 resume once a refresh reaches the host again.
Remote deletion also stays disabled while the worktree holds a live operation
lock. It deletes the remote branch but never destroys an SST stage implicitly.

### Pull request

| key | action |
|---|---|
| `p` | open the PR at the configured `[github].pr_target` |
| `g p` / `l p` | open the PR explicitly in GitHub / Linear Reviews (1.2s chord) |
| `e` | mark a draft PR ready (confirm) |
| `E` | "ship it": mark ready + request `[github].default_reviewer` + arm auto-merge, in one confirm |
| `m` | toggle auto-merge (enqueues into GitHub's merge queue when one is configured) |
| `f` | tail the failing CI checks' logs into the activity pane |
| `v` | reviewer picker (`Space` toggles, `v v` submits) |
| `w` | (review-requests section) check the PR's branch out as a worktree |

### Sessions

Sessions live in a dedicated tmux server; "enter" takes over the terminal, and the same key detaches back to the TUI.

| key | action |
|---|---|
| `F12` | enter the row's coding-agent session (most recent live one, else spawn the primary harness); from another worktree session, switch straight to it; press again to return home; `Ctrl+D` closes it gracefully |
| `Shift+F12` | pick a harness (claude / codex / opencode) for a fresh spawn |
| `Shift+Tab` | cycle the primary harness |
| `F11` | enter the row's diff session (`[diff].command`, default `revdiff`, against the resolved diff base); from another session, switch straight to it; press again to return home |
| `F10` | enter the row's plain shell session; from another session, switch straight to it; press again to return home |
| `Shift+F10` / `Shift+F11` | kill the shell / diff session (confirm) |
| `;` | sessions picker — attach (`; ;`), new named claude (`; c`), new codex/opencode (`; x` / `; o`), graceful close (`; d`), kill (`; x` on a session row) |
| `!` | action picker — run a configured `[[actions]]` entry, `! c` for a custom prompt; `!` on a running action offers to kill it |
| `,` / `.` / `/` | attach the persistent harness session for the wt repo / main clone / dotfiles |
| `>` / `O` | open the wt repo / main clone in Zed |

### Organize

| key | action |
|---|---|
| `l` | section picker (`l l` confirms, `l n` creates a new section) |
| `L` | rename the current section |
| `J` / `K` | move the row (or its whole stack / folded group) down / up |
| `b` | base picker — record which branch this worktree forked from (`b b` confirms; record-only, never rebases) |
| `R` | rebase/restack the selected row — a stack member restacks the whole stack, a standalone worktree rebases onto its recorded base or trunk; same engine as [`wt restack`](stacked-prs.md) (fetch + reconcile + squash-safe replay). On a conflict bail it hands off automatically: `/restack` is injected into the failing worktree's session (cold-starting it if needed) to resolve and finish. Locks per chain, so different stacks/worktrees restack concurrently; members show the sync glyph while it runs (warn-tinted when mid-rebase). Refuses on an already-landed row — that's `c`'s job |

### Automations

| key | action |
|---|---|
| `A` | pause/resume all automations |
| `Ctrl+A` | pause/resume the selected worktree (or its whole stack) |

### Removed-worktrees view (`h`)

`j`/`k` navigate, `p` opens the snapshotted PR, `i` the issue, `y` copies the branch, `Enter` restores the worktree (from the branch if it still exists, else fresh), `h`/`Esc` returns.

## Picker conventions

Every list picker follows the same shape: the key that opened it confirms the highlight when pressed again (`l l`, `; ;`, `' '`, `! !`, `b b`, `v v`), `Enter` always confirms, `Esc`/`q`/`Ctrl+C` always cancel, `j`/`k` move, and digits `1`–`9` quick-pick when the list is short. Special rows get their own letter (`l n` new section, `! c` custom prompt, `; c` new claude session).

## Hub mode

An opt-in alternate UI: a prioritized task inbox instead of the three-pane list, next to a pane that's always a live coding-agent session. Driven by a ⌘ layer (Alacritty-translated, works from either pane): ⌘J/⌘K nav, ⌘L open session, ⌘D/⌘S diff/shell, ⌘H/Esc focus hops, ⌘1-9 task jump; rare classic actions stay plain letters with the task pane focused. `[ui] mode = "hub"` (see [configuration.md](configuration.md#ui)) makes it the default for a bare `wt`; `wt hub` / `wt classic` switch explicitly, and both views share all on-disk state. Full writeup: [docs/hub.md](hub.md).
