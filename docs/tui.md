# TUI guide

When `[issue_tracker].status_command` is configured, the selected worktree's
issue row includes the external tracker's current status, separate from its
agent-asserted work-status banner. A shell action's configured `issue_status`
appears immediately with `(updating)` until its tracked result is confirmed;
failures return to server truth. Normal source spinners and errors apply.
The compact issue line shows a configurable colored icon with `#ENG-123 · In Review`.
An attached GitHub notes issue appears as `#ENG-123 ← #456 · In Review`, without
its own status. Opening/copying still uses the full URLs. Configure exact
status colors and icons under `[issue_tracker.status_styles]`.
Confirmed tracker changes appear in attention as `#ENG-123: In Progress → In Review`;
initial loads and optimistic updates are not narrated as completed changes.
Automation queue entries include their remaining settle delay, followed by running
and outcome entries. External task-action success/failure stays in attention.
Explicit `[[actions]].key` bindings may use lowercase letters or digits in `!`.

`wt` with no arguments launches the TUI. Press `?` inside for the built-in keymap + glyph legend (with `/` to filter it) — that overlay is always the most current reference; this page is the tour. The overlay's title also shows the running version (the source clone's git short hash — see [`wt version`](cli.md#wt-version)).

## Layout

- **List pane** (left; full height by default, capped to 20 rows under `[ui] activity_pane = "full_width"`): one line per worktree — a work-status dot, slug, PR/CI badges, session indicators — grouped into sections, with stacks rendered as trees. By default it spans the full usable height beside both right-hand panes, so a large Inbox keeps its vertical scan space instead of ending above activity output; `[ui] activity_pane = "full_width"` takes the other side of that trade. Remote worktrees participate in those same sections and carry a small monitor indicator; selecting one shows its server in the details pane. Stacked rows carry a tree rail in a gutter to the LEFT of the dot, never in place of it, and the dot column stays straight across stacked and unstacked rows so a scan down it never breaks. The rail is the `tree(1)` idiom — column is depth, glyph is position among siblings (`┌` tops the spine, `├` has one following, `└` is last, `│` continues an ancestor's column), color is lane — and it describes the sub-tree actually on screen, so it never points at a row that isn't there ([stacked-prs.md](stacked-prs.md#the-rail)). There are deliberately no `01`/`02` ordinals, because numbering a fork's children asserts a merge order that doesn't exist (if ordinals are ever wanted, merge **edges** are the thing that actually encodes order). The gutter auto-sizes to the deepest rail drawn and costs zero columns when nothing on screen is stacked. The leftmost slot is the colored **work-status dot** (`wt status` / `u`: red needs-human, yellow needs-testing, green ready, magenta review, cyan working, hollow todo; unasserted rows show the same dim hollow dot as todo; a **hollow dot in a state's color** means the assertion is stale — commits landed after it, so re-verify before trusting it), overridden by the loud git states (busy op, missing, gone, merged); uncommitted changes show as a pencil in the right badge cluster. With `[ui] sort = "status"` (default), rows auto-sort inside each section by that urgency — the cursor follows the worktree, not the position. Fresh **merge edges** (`wt edge`, [cli.md](cli.md#wt-edge-from-kind-to)) then topologically order rows within their section, so rendering order reads as merge order — sections stay the human's batching, edges own order within a batch, and a stale edge (either branch moved) silently stops steering. A pinned "review requests" section surfaces PRs waiting on your review; press `d` to dismiss the selected snapshot until that PR changes.
- **Folded-section summary**: when the cursor sits on a folded section header, the details pane describes the BATCH rather than a worktree — a work-status rollup (`7 worktrees · 2 ready · 1 needs-human · …`, most urgent first, each dot in its state's color), a line of mechanical facts that decide whether the batch can move (open PRs, merge-queue entries, failing checks, dirty checkouts, paused automations — each omitted when zero), the member rows rendered with the same gutter and badge glyphs the list uses, `low`/`medium`/`high` on the `ready` ones, and finally the verbatim notes of any member blocked on you. Risk and the badge cluster each get a column reserved at the section's widest member, so both read straight down the pane; without that the risk label drifted by the badge-count difference between rows, since the cluster is flush right and renders only the badges a row actually has. Those notes are sized to the pane: they split whatever rows the rest of the summary left over, so one blocked member reads in full where five each get a couple of lines, and a note the budget cut ends in `...` — a note that just stopped mid-sentence read as a rendering fault, and read as the WHOLE note to anyone who didn't know a cap existed. Member rails are laid out over the members shown here, same rule as the list, so a parent outside the section simply isn't part of the spine. A section is whatever batch you dragged into it, so the summary scrolls on the usual `Ctrl+J`/`Ctrl+K` with the key hints pinned below it.
- **Details pane** (right): the worktree's **slug in the border bar** (lowercase, like every other pane's border — it is the identifier `wt status`, a manager message and a log line all use, and it used to appear here only as the tail of the `path` row), then its resolved **title** on the first body line (best source wins — `llm > pr > commit > slug` — with a muted `(source)` tag), then a full-width **work-status banner** at the top (state, risk, age, and the complete note, word-wrapped in a mid-tone behind a thin `│` blockquote rail in the state's color, centered under the status dot — the same dot shape/colors as the list; the `u` picker shows the same glyphs per state), then the configured rows (`[ui].rows` in [configuration.md](configuration.md#ui)) for the selected worktree — branch (as `<branch> → <base>`, one line for one fact), tracker issue, stage, PR, sessions, git state — then a rebase-state block (restacking / mid-rebase / conflict with the clashing files) when something is moving, plus the harness-generated description band when `[naming]` is configured (the generated title feeds the border bar and list labels). When the row's session just wrapped up, the harness's own summary line renders muted above that description (it disappears as soon as the conversation moves on) — for Claude that includes the "※ recap" away-summaries, hint stripped.
- **Activity pane** (below details by default; `[ui] activity_pane = "full_width"` moves it back across the bottom under both panes, capping list+details at 20 rows in exchange for the full terminal width before the feed word-wraps): live outputs — harness sessions, action runs, and two event feeds: the curated **attention** feed (status transitions, needs-you signals, new PR comments from other people, dev-server startup crashes, errors) and the full firehose. It occupies the lower part of the right column beneath worktree metadata rather than spanning below the Inbox. Dev logs stay out of this layout: on a row with a running, starting, or crashed dev server, `! l` opens its live supervisor output in a dedicated scrollable overlay (`j`/`k`, arrows, page keys, `g`/`G`, mouse wheel); closing the overlay stops its one-second poll. A supervised dev server that exhausts its startup retries contributes its last useful application-error line and points to `wt dev logs`, rather than only turning the row red. The attention feed is the default whatever row is selected — navigating never flips the pane to a session's output; only a destroy in flight or a just-launched action takes over. Attention lines **word-wrap** (their notes are the payload — ready risk notes, needs-human asks — and continuation lines run the full pane width under a two-cell hanging indent, so a long note isn't squeezed into the column right of the time+source gutter). Each FIELD of a status line is clamped to its own documented budget before it gets there, though — the note to the ~400 the CLI teaches (so a note written to spec is untouched), the gate and the verification steps to a headline — because the pane is shared and word-wrap means an unbounded field doesn't truncate, it floods: one 1896-character `verifyAfterMerge` wrapped to fourteen lines and evicted every other row's signal from the feed. The clamp always marks its cut, and the full text is in the details pane and the log file. The firehose and destroy views stay one line per event for scannability, with the full text always in the log file. `'` picks an output explicitly (remembered per worktree until that output dies), `[` / `]` cycle, `"` jumps to attention (again for the firehose), `Esc` forgets the pick and returns to the default. The feeds **survive restarts** — at boot they're restored from the daily app log (yesterday + today, up to the buffer caps), so the attention trail is still there after wt (or the machine) bounced; identical lines written within a few seconds of each other (several wt processes observing the same transition) are collapsed to one on restore. Scroll them with `Ctrl+Shift+J`/`Ctrl+Shift+K` (or `Ctrl+E`/`Ctrl+Y`, or the mouse wheel — plain `Ctrl+J`/`Ctrl+K` scrolls the details pane); the view re-follows the live tail when you return to the bottom. Once you've worked through what the attention feed is asking for, `x` (while the feed is showing) **marks it seen**: everything up to that moment drops to dim below a `── seen HH:MM:SS` rule, and the pane snaps to the live tail (re-engaging follow if you'd scrolled back), so the feed reads "only new stuff" at a glance while the handled history stays scrollable — nothing is deleted, and the firehose is untouched. The watermark persists (wtstate), so the boot restore comes back already dimmed; an all-dim tail ending in the rule means you're caught up.
- **Footer**: transient content on the left — the active **toast** (keystroke acks like "copied branch", plus background completions: work-status changes, automation fires, action results) or a quiet `? help` hint when idle — and the four special-session buttons grouped at the right: `[m]` the [manager](manager.md) first, then `[.]` the main clone, `[,]` the wt repo, `[/]` dotfiles (absent when there's no dotfiles repo), each key colored by that session's live state (dim when none). When a live manager claude session has produced a turn, its **context %** renders immediately left of `[m]` (dim; warn at ≥70, red at ≥85 — compact via `M m` before Claude auto-compacts it mid-thought). Replaced by a text prompt when one is active (`n` local new-worktree, `Ctrl+N` remote new-worktree, `L` rename section). Background toasts are always also a line in the bottom pane's feeds — the toast is the flash, the feed is the record — while keystroke acks are toast-only (they answer a key you just pressed).

**New PR comments land on the attention feed.** When someone else comments on a worktree's PR (a top-level comment or a review body), the line shows up as `<login> commented: <first ~100 chars>` under that worktree — nothing in git moves when a coworker types, so without this the comment lives only in the details pane. Bots and your own comments are filtered out, and a comment is narrated once: the first observation after startup is treated as history, so you get the backlog that arrived while wt was down but never a replay of the whole conversation (more than three at once collapse to a single `N new PR comments (…)` line). Inline review-thread replies aren't included — the details pane's unresolved-thread count covers those.

Freshness is push-based: fs watchers on git refs, worktree dirs, locks, and the state files — plus the optional [GitHub webhook daemon](github-events.md) — invalidate exactly what changed. `r` re-fetches as a backstop; `Ctrl+R` (with confirm) nukes all cached data and refetches from scratch. GitHub-side changes have no local signal at all, so the PR fetch also re-runs every 3 minutes (or on the daemon's own backstop when it's configured) — that interval is the worst case for how late a comment can reach the feed.

## Keymap

### Navigation

| key | action |
|---|---|
| `j`/`k`, arrows | move cursor. Cursor lists keep vim's `scrolloff` of 3 rows: the view starts sliding a few rows before the cursor reaches an edge, instead of parking it on the edge for the rest of the list |
| `g` / `G` | jump to top / bottom |
| `Space` | jump to the next row needing attention (`needs-human` / `needs-testing` / `ready`), scanning forward and wrapping — the cross-section scan that per-section status sort can't express |
| `Tab` | fold/unfold the section under the cursor — a manual section, the Inbox, or the Archived block. The fold persists (`foldedSections` in wtstate), and folding collapses the group's cursor stops as well as its rows, so `j`/`k` never walk through something you can't see. Archived is otherwise not a section: it isn't in `sectionsOrder`, stays pinned to the bottom, and its summary offers no rename or move because it has neither |
| `Ctrl+D` / `Ctrl+U` | jump to the first visible item in the next / previous section. For an expanded section that is its first row; for a folded section it is the selectable title, ready for `Tab` to unfold |
| `Ctrl+J` / `Ctrl+K` | scroll the details pane, 3 rows a press |
| `Ctrl+Shift+J` / `Ctrl+Shift+K` | scroll the bottom event feed — same 3-row step (also `Ctrl+E`/`Ctrl+Y`, mouse wheel); re-follows at the bottom. Kitty-protocol terminals only: legacy encodings can't express the chord and it degrades to the details scroll, leaving `Ctrl+E`/`Ctrl+Y` for the feed. There is no `Alt+J`/`Alt+K` alias, deliberately: outside the kitty protocol `Alt+<letter>` and `Esc`-then-letter are the same bytes, so such a binding hijacks bare `j`/`k` whenever you navigate right after dismissing a modal (or when a terminal binding emits an Esc-prefixed letter) |
| `h` | flip to removed-worktree history (grouped under day headers; a day starts at 04:00 local). Rows show the saved work-status glyph on the left and PR/removal glyph on the right; older entries without a saved status show `?`. Selecting a row shows its saved status and note. No remote status lookup runs for history. |

**Where the cursor goes when the row under it leaves.** The cursor is
anchored to a row, not to a position, so it follows a row that merely
re-sorts (a status change, `Shift+J`/`K` dragging it, a restack). But
four actions take the row OUT of the slot you were reading — `d`, the
`c` sweep, `a`, and filing it elsewhere with `l` — and there the cursor
holds the PLACE instead: it lands on the next surviving row in the same
section, or the previous one when the row was last, and only leaves the
section when the whole section is going. It never lands on a row the
same sweep is about to destroy, or on one already mid-teardown. This
matters most for `d` and `c`: those park the row in the archived block
at the bottom of the board for the seconds their background remove
takes, so a cursor that followed it would drag you off your section and
strand you next to the archive. Restoring from the archive with `a` is
the one that still follows the row — it's coming back to where you can
work on it.

### Worktree actions

Creation selects the actual row once inventory has rendered it. Initial bottom
placement overrides the current sort for that row until its work-status claim
or manual layout changes; later status updates use the normal ordering.

| key | action |
|---|---|
| `n` / `N` | new local worktree prompt (accepts an issue id + optional title words, a tracker URL, branch, or slug, plus `--attach`, `--gh <n>`, `--any`, `--base <ref>` — same resolution as [`wt new`](cli.md#wt-new-id-titleurlbranchslug)); `N` pre-fills `--base` with the selected row's branch. On success the section expands and the cursor lands on the new row at the bottom of its section; on a resolution failure the prompt reopens with your input intact |
| `Ctrl+N` | create on `[remote]`; immediately expands Inbox and selects a temporary creating row, then selects the completed worktree. Its section expands and the new row starts at the bottom. The worktree appears in its normal section (or Inbox) with a small remote indicator, and F10/F11/F12 route that row's sessions over SSH |
| `o` | open the worktree in your editor (`[editor] command`; default Zed) |
| `d` | remove locally or on the row's remote host (confirm; escalates to a force-remove warning listing every hazard when dirty/unpushed) |
| `c` | clean all merged/gone worktrees across the local and configured remote fleets (one combined confirmation). Never forces: a candidate holding uncommitted changes or unpushed commits — or a landed row still owing its [`verifyAfterMerge`](cli.md#wt-status-slug-state--m-note---risk-r) check — is shown as `kept` in the confirm list and survives the sweep; use `d` on it deliberately. Hazards render as a bare phrase, never with the field behind them: every reader of one is a scan line (a modal row, a `d` confirm that comma-joins reasons and appends *will be lost*, a toast), and `verifyAfterMerge` is the one field with no length budget, so inlining it buried the hazards next to it. Press `V` on the row to read the steps. Same for the `builtin:clean` automation, which has no human in the loop at all |

Both read "unpushed" as commits missing from `origin/<branch>` — the `(↑n ↓m)` group, not the `[↑n ↓m]` one. A branch that is fully pushed but ahead of its base is not at risk and neither path treats it as such; a landed branch whose remote ref was pruned isn't either, since a squash merge is what left its local commits behind.
| `a` | archive / restore the row, local or remote. Archiving folds the Archived block so the row disappears immediately; `Tab` on its header expands it for restore. Archive placement belongs to this TUI's local fleet ledger, while a remote checkout remains untouched on its host |
| `i` | open the primary tracker issue when it has a URL, otherwise the attached GitHub issue (`wt issue --gh`) |
| `I` | open the primary tracker issue (needs `[issue_tracker]` with a URL template, or a `gh-`prefixed slug id) |
| `#` | set the worktree's tracker id — a footer prompt seeded with whatever the row resolves to today; `Enter` saves, **an empty line asserts the worktree has no tracker issue**, `Esc` cancels. Emptying a field that was seeded with the current answer is the natural way to say "not this one", and it has to be a stored none rather than a cleared override: on a slug that carries an id — the population most likely to be wrong — dropping the override just re-supplied it from the slug, so the prompt was a no-op on exactly the rows you would want to detach. `wt issue <slug> --clear-id` is the way back to the derived value. Validated (`COZ-2185` shape), stored per-slug, and preferred over the slug everywhere: the issue row, `i`/`I`, `{{issue_id}}`, and `requires = ["issue.tracker"]`. This is how a worktree named for the work rather than the ticket gets one |
| `s` | open the deployed stage URL, or the running `[dev_server]` URL when no stage is deployed |
| `t` | regenerate the AI summary |
| `V` | expand / collapse the row's [`verifyAfterMerge`](cli.md#wt-status-slug-state--m-note---risk-r) steps in the details pane. The field is dormant until the branch lands, so the block starts collapsed to a header plus a two-line preview and opens by itself once the check has come due; this flips whichever applies, and resets when the cursor moves. Collapsing is the point: a 1896-character field wrapped to fourteen lines pushed the note, the gate and every definition row below the fold, and it was rendering that way on the one row that could not act on it yet |
| `y` | yank picker — copy branch (`b`), stage (`s`), stage URL (`S`), dev-server URL (`d`), path (`p`), slug (`n`), preferred issue (`i`, tracker URL first, then attached GitHub issue), primary tracker issue (`I`), PR URL (`r`); a full picker since the rebuild: `j`/`k` move, `1`–`9` quick-pick, `y`/`Enter` confirm the highlight, direct letters still fire immediately. On a folded section header the same key yanks the BATCH instead: name (`n`), member slugs (`s`), member branches (`b`), and a pasteable list (`l` — the name, then one `- <slug>: <title>` line per member). Slugs and branches are space-joined so they drop straight into a command; the list is the form a message to the manager wants, which is why it exists |
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
| `E` | "ship it": mark ready + request `[github].default_reviewer` + arm auto-merge, in one confirm (the reviewer leg is omitted when `[github].reviewers = false`) |
| `! m` | Arm/disarm "merge when ready" from the `!` picker, directly and without confirmation. Arming checks the PR's base branch: a branch with a merge queue uses `enqueuePullRequest`; otherwise it uses classic auto-merge. Cancellation instead checks the PR's actual queue entry and auto-merge request, since a queue-base PR can be classically armed but not queued. Pending required checks are retried in the background against the same head SHA; `! m` again cancels that retry. On repos that permit it, classic auto-merge can arm while the queue waits. An armed-but-not-queued PR uses the list's queue icon without a position; a queued PR shows its position. The details pane retains its separate auto-merge segment. |
| `f` | tail the failing CI checks' logs into the activity pane |
| `v` | reviewer picker (`Space` toggles, `v v` submits; disabled when `[github].reviewers = false`) |
| `w` | (review-requests section) check the PR's branch out as a worktree |

### Sessions

Sessions live in a dedicated tmux server; "enter" takes over the terminal, and the same key detaches back to the TUI.

| key | action |
|---|---|
| `F12` | enter the row's coding-agent session (the selected primary harness when live, else another live harness, else resume that harness's mapped primary conversation or spawn it when none exists); from another worktree session, switch straight to it; press again to return home; while attached, `Ctrl+D` closes it gracefully |
| `Shift+F12` | pick a harness (claude / codex / opencode) for a fresh spawn |
| `Shift+Tab` | cycle the primary harness |
| `F11` | enter the row's diff session (`[diff].command`, default `revdiff`, against the resolved diff base); from another session, switch straight to it; press again to return home |
| `F10` | enter the row's plain shell session; from another session, switch straight to it; press again to return home |
| `Shift+F10` / `Shift+F11` | kill the shell / diff session (confirm) |
| `;` | sessions picker — attach (`; ;`), new named claude (`; c`), new codex/opencode (`; x` / `; o`), graceful close (`; d`), kill (`; x` on a live session row — fires directly, no confirm; getting there already took two deliberate steps). Codex rows use wt's stable `primary` / `2` / `3` names rather than Codex-generated thread summaries. |
| `!` | action picker — identical on local and remote rows (remote execution uses SSH): run a configured `[[actions]]` entry, `! c` for a custom prompt; `!` on a running action offers to kill it. Two agent-delegation builtins are pinned at the top: `! u` has the row's agent re-assess and assert `wt status`, `! g` has it continue the work per the current status (both send to the primary harness session, cold-starting it if needed); with `[dev_server]` configured, start/restart (`d`), stop (`s`), and scrollable logs (`l`) are pinned below them. `! m` toggles auto-merge (group "github") |
| `,` / `.` / `/` | attach the persistent harness session for the wt repo / main clone / dotfiles (`[paths] dotfiles`, default `~/.dotfiles`). The dotfiles slot is dropped entirely when that directory doesn't exist, freeing `/` and `\` |
| `<` / `>` / `\` | slot command palette for the wt repo / main clone / dotfiles session — the shift analog of the attach key (dotfiles rides `\` because shift+`/` is `?`, help). Entries: continue current work (`g`), `/compact` (`m`, fires directly), open the slot in your editor (`z`), custom free-text message (`c`). Prompt entries send to the slot's session, cold-starting it detached if needed |
| `m` | attach the [manager session](manager.md) — the singleton fleet coordinator |
| `M` | [manager command palette](manager.md#the-command-palette-m) — digest (`d`), triage needs-human (`t`), merge order (`o`), nudge stalled (`n`), audit statuses (`a`), start next todo (`s`), ask about the selected row (`r`), `/compact` (`m`), custom message (`c`); user `[[actions]]` with `target = "manager"` appear too. Fleet commands report back via `wt manager report`, which lands on the attention feed |

Inside these four special sessions, `F10`/`F11`/`F12` all return to wt — slots aren't worktrees, so there's no shell or diff sibling to switch to.
| `O` | open the main clone in your editor (the wt repo's editor open lives in its palette: `< z`) |
| click link | open an OSC 8 hyperlink in a wt-managed tmux session; tmux handles the link itself so mouse-enabled Codex does not swallow the terminal's normal click action |
| mouse drag | select text in a wt-managed tmux session and copy it automatically to the macOS clipboard on release |

### Organize

| key | action |
|---|---|
| `l` | section picker (`l l` confirms, `l n` creates a new section) |
| `L` | rename the current section |
| `J` / `K` | move the row (or its whole stack / folded group) down / up — under status sort, within the same status rank only |
| `b` | base picker — record which branch this worktree forked from (`b b` confirms; record-only, never rebases) |
| `u` | work-status picker (`u u` confirms; `t`/`w`/`r`/`n`/`h`/`y`/`v`/`d` set the state directly, `x` clears; `m` picks the highlighted state and collects an optional note in the footer — Enter on an empty note is a plain pick, Esc cancels the whole pick). A second `ready` row, `a` — **`ready + verify after merge`** — sets [`verifyAfterMerge`](cli.md#wt-status-slug-state--m-note---risk-r) instead: the footer collects the STEPS (pre-filled with whatever the row already owes, so the same row amends), and the obligation survives the merge, keeps the row rendering as `needs-testing` once the branch lands, and holds it back from the `c` sweep until someone asserts `verified`. Emptying that pre-filled box is how you take the obligation back off a branch without claiming it was verified; the toast says which happened. Same record as [`wt status`](cli.md#wt-status-slug-state--m-note---risk-r), minus the CLI's risk/note rules (you're the human it escalates to) |
| `R` | rebase/restack the selected row — a stack member restacks the whole stack, a standalone worktree rebases onto its recorded base or trunk; same engine as [`wt restack`](stacked-prs.md) (fetch + reconcile + squash-safe replay). On a conflict bail it hands off automatically: `/restack` is sent to the failing worktree's session (cold-starting it if needed) to resolve and finish. Locks per chain, so different stacks/worktrees restack concurrently; members show the sync glyph while it runs (warn-tinted when mid-rebase). Refuses on an already-landed row — that's `c`'s job |

### Automations

| key | action |
|---|---|
| `A` | pause/resume all automations |
| `Ctrl+Shift+A` | cancel all queued automations, leaving running actions untouched |
| `Ctrl+A` | pause/resume the selected worktree (or its whole stack); in the `h` history, the selected archived row |

### Perf overlay (`P`)

Answers one question: *the machine feels slow — is that us?*

A filtered `btop` scoped to everything descending from the wt process or
its private tmux server. The headline is a verdict line (wt's share of
the CPU actually in use, not of installed capacity — the latter reads
reassuringly small on a 12-core box even when wt owns all of it),
followed by system meters, a breakdown by category (agents, tests,
typecheck/lint, dev servers, wt, tmux, shells), a breakdown by worktree
session, and the heaviest processes both inside and outside wt's tree.
That last block is the point: when the hog is a browser tab, it says so
instead of sending you hunting through worktrees.

| key | action |
|---|---|
| `P` / `Esc` / `q` | open / close |
| `j` / `k` | scroll (the shared overlay keymap: `PgUp`/`PgDn` half-page, `g`/`G` top/bottom) |
| `i` | send the snapshot to the wt-source session (`,`) as an investigation prompt, then enter that session |
| `r` | resample now |

The overlay also hunts for **leaked headless wt instances** — processes
orphaned to launchd when a terminal died without the process exiting
(the SIGHUP handler makes current builds exit; older builds and wedged
teardowns can survive). Any found get a verdict-level warning plus a
LEAKED section listing pids, CPU, and a ready-to-run `kill` line —
they'd otherwise keep polling GitHub and duplicating attention-feed
lines invisibly. The `i` investigation prompt includes them.

Sampling runs only while the overlay is open (every 2s, four shell-outs)
and stops entirely when it closes — nothing polls in the background, and
the snapshot is never persisted to the query cache.

The same snapshot is available headless as [`wt perf`](cli.md#wt-perf---json)
(`--json` for the raw structure) — the default output is the `i`-key
report, so an agent outside the TUI can be handed one command instead
of a screenshot.

Two accuracy notes. CPU percentages come from `ps` `%CPU`, which is a
**lifetime decaying average, not an instantaneous sample** — a process
showing 130% may be idle right now. Read it as sustained pressure; the
overlay is not a profiler. Memory "used" is computed from `vm_stat` as
active + wired + compressor pages (Activity Monitor's definition) rather
than `os.freemem()`, which counts only genuinely free pages and so reads
~90% used on any machine that's been up a while.

Unrelated but adjacent: `WT_PERF=1 bun src/main.ts` arms an event-loop
lag probe that logs whenever wt's own render thread blocks. That's the
tool for "j/k feels laggy"; this overlay is the tool for "the whole
machine feels slow".

### Error overlay

Unhandled errors in the TUI process (uncaught exceptions, unhandled
promise rejections, React render errors) are **captured instead of
printed** — a raw stack trace on stdout/stderr while the renderer owns
the terminal garbles the panes. Captured errors go to a small in-memory
ring (last 5) plus the daily log (full stack), a footer toast flashes,
and this overlay pops automatically. It has no opening key: if another
modal is open it waits its turn and pops when that modal closes;
dismissing acknowledges everything shown, so only a *new* error re-pops
it.

| key | action |
|---|---|
| `j` / `k` | scroll the stack (shared overlay keymap: `PgUp`/`PgDn` half-page, `g`/`G` top/bottom) |
| `i` | send the error to the wt-source session (`,`) as an investigate-and-fix prompt, then enter that session |
| `y` | copy the error (origin, timestamp, full stack) to the clipboard |
| `Esc` / `q` | dismiss (acknowledge) |

An **uncaught exception does not kill wt** — the process keeps running
(the state sources are re-derived queries that self-heal), but the
overlay shows a "state may be inconsistent; restart when convenient"
banner for the rest of the run. Identical back-to-back errors collapse
into one entry with a `×N` counter rather than flooding the ring. A
crash *while rendering* can't use a modal (the app tree is gone), so it
gets a minimal full-screen crash view instead: `r` retries the render,
`y` copies, `q` quits cleanly.

Test hook: `WT_DEBUG_THROW=1` (or `=rejection`) fires a synthetic
error ~1.5s after startup — that's how the capture path is probed.

### Removed-worktrees view (`h`)

`j`/`k` navigate, `g`/`G` jump to top/bottom, `p` opens the snapshotted PR, `i` the issue, `y` copies the branch, `Enter` restores the worktree (from the branch if it still exists, else fresh), `h`/`Esc` returns.

## Picker conventions

Every list picker follows the same shape: the key that opened it confirms the highlight when pressed again (`l l`, `; ;`, `' '`, `! !`, `M M`, `< <` / `> >` / `\ \` in the slot palettes, `b b`, `v v`, `u u`, `y y`, and `Shift+F12` again in the harness picker), `Enter` always confirms, `Esc`/`q`/`Ctrl+C` always cancel, `j`/`k` move, and digits `1`–`9` quick-pick when the list is short — except the action picker and the manager/slot palettes (assigned keys instead) and the reviewer picker (`Space` toggles; digits would be ambiguous in a multi-select). Rows with a natural name carry a direct letter chord, shown dim in the row (`u t` → todo, `u y` → ready, `; c` new claude session); special rows get their own letter too (`l n` new section, `! c` custom prompt).

Confirm modals follow the same muscle-memory rule in reverse: the key that opened one also **cancels** it (`d`, `c`, `e`, `E`, `w`, `!`'s kill confirm), alongside the universal `n`/`Esc`/`q`/`Ctrl+C`.

Every text input (the `n`/`N`/`Ctrl+N` new-worktree prompt, `L` section rename, `u m` / `u a` status text, `! c` custom prompts and action args, `; c` session names, help search) shares one line editor: `←`/`→` move the cursor, `Home`/`End` (or `Ctrl+A`/`Ctrl+E`) jump to the ends, `Opt/Alt+←`/`→` (or `Esc B`/`Esc F`, or `Ctrl+←`/`→`) jump by word, `Backspace`/`Delete` edit at the cursor, `Opt/Alt+Backspace` deletes the word left, and `Ctrl+U`/`Ctrl+K` kill to the start/end of the line (`Ctrl+U` is how you empty a pre-filled prompt, since backspacing past empty backs out instead). Word boundaries are slug- and sentence-aware: `-`, `_`, and spaces all separate words. Backspace on an already-empty input still backs out of the prompt.

The title bar's `auto ⏸` chip (inverse, warn-colored) means all automations are paused — a fleet-tier fact deliberately louder than the CPU/usage telemetry next to it.
