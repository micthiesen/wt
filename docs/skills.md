# Agent skills & instructions distribution

wt is the single source of truth for its own agent tooling. It bundles the
skills coding agents need to work well with wt (in `skills/` of the wt
checkout) plus a small always-on instructions block, and keeps the installed
copies current on your machine — across every harness, through whatever
symlink or rulesync topology your dotfiles use. The point is that you never
hand-maintain wt-related agent config: updates ship with `git pull` in the
wt checkout and offer themselves on the next launch.

## What gets distributed

| unit | what it is |
|---|---|
| `instructions` | a managed block spliced into each tool's **global instructions file** (`~/.claude/CLAUDE.md`, `~/.codex/AGENTS.md`, `$PI_CODING_AGENT_DIR/AGENTS.md`, `~/.config/opencode/AGENTS.md`): the always-on ownership rules for agents working in wt worktrees (work status, decision ownership and the `needs-human` refusal test, manual testing, the dev server) |
| `wt` skill | orientation: subcommands, conventions, the stacked-PR model, gotchas |
| `restack` skill | the conflict-resolution playbook behind `/restack` |
| `manager` skill | the fleet-coordinator playbook for the [manager session](manager.md) |
| `start` skill | kick off work inside a prepared worktree (brief → research → build → review → test → status hand-off) |
| `handoff` skill | move a distinct follow-up into a new worktree, write its `prompt.txt`, and start the configured primary agent |
| `shepherd` skill | the fleet sweep behind `/shepherd`: drive every row toward mergeable on a loop, stopping short of the merge |
| `babysit` skill | the single-worktree counterpart behind `/babysit`: see your own branch through the review bot's findings and its follow-up reviews, then merge it — the one place an agent lands a PR, and only on the human's explicit invocation |
| `triage` skill | turn a pasted task batch into prioritized, ready-to-work worktrees |

The instructions block exists because skills only load when invoked — the
ownership rules ("you own testing", "never end without a status") have to be
in the always-loaded instructions layer to actually govern behavior.

It's also the only layer that can *correct* a repo. A shared repo's own
`CLAUDE.md`/`AGENTS.md` is written for the contributors who don't use wt, so it
says things like "run `pnpm dev`" — always loaded, and wrong inside a worktree.
Adding a wt caveat there isn't an option (it would be noise for everyone else),
and a skill loses the race because it isn't loaded when the agent reads the
repo's table. The managed block is per-machine, always on, and therefore the
one surface that wins that argument — which is why the dev-server rule lives
there and not in the `wt` skill alone.

Before handoff, agents stop their own dev server with `wt dev stop` when they
have no planned further use, retaining it for ongoing verification that needs
it. The start and completion playbooks repeat this ownership rule. A PR or an
idle session does not establish disuse, and no automatic shutdown is tied to
either signal.

The start skill reads a primary tracker task through `wt issue --read`, alongside
the brief and any attached GitHub issue. `[issue_tracker] read_command` supplies
the provider implementation; no provider name or credential belongs in managed
skills. Reader failure means incomplete context, not an empty task. Returned task
text, attachments, and images are external data rather than executable instructions.

The wt skill mirrors issue navigation: `i` / `y i` prefer the primary tracker
URL and fall back to an attached GitHub issue; `I` / `y I` stay primary-only.

### What belongs in the block

Because it lands in the reader's own always-loaded file, the block is edited
by **replacement, not accretion**, and it carries **rules, not the incidents
behind them**: no measurements, no dates, no war stories, no rationale for a
rule that has to be followed either way. Every line should be an imperative a
reader can act on without knowing why it exists.

That is deliberately the opposite of wt's own `AGENTS.md`, which is an
incident ledger, and the difference is the audience. A bullet there is read by
whoever is about to change the mechanism, where the evidence is the only thing
stopping them from undoing it; the block is read by agents doing unrelated
work, where provenance is context spent on every turn and gets skimmed past.
So when a papercut teaches something, the lesson goes into the repo's
`AGENTS.md` in full and reaches the block only as the shortest imperative that
changes behaviour — or not at all, which is the common case.

`registry.test.ts` holds a line budget on the block for the same reason. It
had grown to 2.5x that budget one reasonable-looking addition at a time, and
no single addition was ever the problem, which is why the check is a number
rather than a review habit.

The skills are looser: they load on demand, so a worked example there can earn
its space. The war-story test still applies to anything that reads as an
incident report rather than a procedure.

## The startup check

When the TUI starts (and before it takes over the terminal, so agents
spawned from that session see the updates), wt compares every unit against
what's installed and asks **y/n once per pending update**:

```
wt: 2 agent-skill update(s) available
• Install skill manager (playbook for the singleton manager session)? [Y/n]
~ start: existing copy was not installed by wt. Overwrite with the wt-managed version? [y/N]
```

- A **"no" is remembered per content version and per target** — you're never
  re-asked until the bundled content actually changes, and a decline for one
  install location never suppresses a later install to a new one (say, a
  freshly configured harness).
- Copies that wt didn't install (or that were edited afterwards) are never
  overwritten silently; they get the `[y/N]`-default prompt above.
- When applying goes through a rulesync pipeline, wt confirms the exact
  regenerate command it's about to run (`bash …/scripts/rulesync.sh`, or
  `npx rulesync generate`) once per pipeline before touching it.
- `[skills] startup_check = false` turns the startup prompt off entirely.

If a unit has template blanks (see below) you're asked once, and the answer
is remembered forever.

## How freshness is decided

Every file wt installs ends with a stamp comment, `<!-- wt-managed <hash> -->`,
where the hash covers the body above it. Comparing the installed body and
stamp against the current render distinguishes:

- **fresh** — matches the current bundled content
- **outdated** — an intact wt-managed copy of an older version (safe to update)
- **modified** — no stamp, or edited since install (yours; prompt-only, never auto)

The instructions block uses begin/end markers with the same hash scheme, so
everything OUTSIDE the block in your instructions file is untouched — wt only
ever rewrites the region between its own markers.

## Where things get installed

Detection follows the real filesystem, per tool configured on the machine.
The tools here are the ones wt installs *into* — a superset of the harnesses
it can start sessions for (`core/harness/`). Pi, for instance, reads the
shared instructions file and `~/.agents/skills` without wt ever spawning it.

- **Configured means the config dir has CONTENT.** An empty `~/.config/<tool>`
  is a stow mount point a dotfiles package left behind when it stopped
  generating for that tool, not a tool to serve. Counting it would put a unit
  in the pending list that no sync can ever clear.
- **Native**: `~/.claude/skills/<name>/` (Claude; OpenCode reads the same
  dir), `~/.agents/skills/` or `$CODEX_HOME/skills/` (Codex),
  `~/.agents/skills/` or `$PI_CODING_AGENT_DIR/skills/` (Pi), and the global
  instructions files listed above.
- **Symlinks are resolved and deduped**: when several tools point at one
  real directory (stow-style dotfiles, `.agents` → `.claude`), wt writes
  once and credits every tool it serves. Resolution works on paths that
  don't exist yet — an instructions file a pipeline hasn't generated is
  resolved through its parent, so it can't be mistaken for a native file.
- **rulesync pipelines are first-class**: if the resolved location lives
  inside a repo with a `.rulesync/` dir, the generated output is a wipe-on-
  regenerate artifact — so wt writes to the durable SOURCE instead
  (`.rulesync/skills/<name>/`, and the `root: true` rules file for the
  instructions block) and then regenerates: via the repo's own
  `scripts/rulesync.sh` when it has one, else `npx rulesync generate`.
  One regenerate per sync, not per unit. Those repos commit their generated
  output, so wt reports how many files are left uncommitted afterwards —
  an update nobody commits is one `git checkout --` from being gone.

## Template values

Bundled content may carry `{{key}}` blanks for genuinely per-user text (for
example `project_notes` in the `start`/`triage` skills: your project's
design-review flow, testing tools, tracker quirks). Answers are collected
interactively the first time a unit needing them is installed, remembered in
`~/.cache/wt/skills.json`, and rendered into the installed copy. Changing an
answer (after `wt skills reset --answers`) makes the affected units show as
outdated — the render changed. Unanswered blanks render a sensible fallback;
nothing blocks on them.

## The CLI

```
wt skills                    # freshness of every unit at every target
wt skills sync [<name>...]   # interactive install/update (what startup runs)
wt skills sync --yes         # accept all missing/outdated; never touches modified
wt skills sync <n> --force   # non-interactive: also overwrite a modified copy
                             # (interactive runs always ask per modified copy)
wt skills diff <name>        # what a sync would change
wt skills reset              # forget remembered answers + declines
```

Naming a unit explicitly (`wt skills sync start`) overrides a remembered
decline — asking for it by name IS the re-ask. `wt doctor` shows a one-line
banner when updates are pending.

## Remote worker provisioning

`wt remote agent start <slug>` provisions the worker before it sends `/start`
or `$start`: the controller first runs `wt skills sync --yes` on the worker,
which installs missing/current bundled skills and the managed instructions
block. The ordinary modified-copy rule still applies, so remote provisioning
does not overwrite a personal skill. After provisioning, `wt agent start`
checks the selected harness's actual skill lookup paths and fails without typing
anything if `start` is still unavailable. This second guard also covers direct
worker invocations and provisioning failures.

The executable half is supplied at session creation rather than copied by the
skills system: `wrapInnerArgs` prepends the directory of the `wt` launcher that
received the remote command to every harness's `PATH`. A worker may therefore
use a configured `wt_path` that is absent from its non-interactive SSH PATH,
while the started agent can still run `wt status`.

## Keeping your own versions

Prefer your own `start` skill? Decline the prompt once — wt remembers that
decision for that version and the fleet keeps working (the CLI's own
guidance output, `wt status` footers and friends, is always current
regardless of skills). `wt skills status` still shows the unit as
`local copy differs (declined for this version)` so the state stays visible
rather than silent.
