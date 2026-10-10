#!/usr/bin/env bash
# fixture.sh — build a SEALED wt instance whose board carries every shape
# the renderer branches on, then drive a probe against it.
#
# Why this exists: a conditional affordance renders nothing when its input
# is absent, which looks exactly like one that's broken. A board with no
# stacked-and-co-located rows can't show the stack rail; a board whose
# worktrees have no PRs can't show PR badges. Four "regressions" got
# reported off one such board in a morning, three of them were the absent
# input, and the only thing that settled it was CONSTRUCTING the input.
# That should take a minute, not an argument — so this builds a throwaway
# repo, a config pointing at it, and a fabricated fleet:
#
#   scripts/fixture.sh build              # (re)build the fixture from scratch
#   scripts/fixture.sh probe [name] [w] [h]   # build if needed + start a TUI probe
#   scripts/fixture.sh wt <args...>       # run any wt command against it
#   scripts/fixture.sh env                # path to a sourceable env.sh
#   scripts/fixture.sh rm                 # delete it (and kill its tmux servers)
#
# Everything lives under $TMPDIR/wt-fixture (override: WT_FIXTURE_ROOT).
# It is sealed the way docs/configuration.md describes — own config, own
# cache root, own WT_TMUX_SOCKET — so it shares NOTHING with the live
# instance and the probe harness's read-only rules don't apply: destroy
# rows, confirm prompts, exercise anything.
#
# It drives the workspace's built native binary, so the fixture
# reflects the change you're validating, and it builds the board through
# public CLI commands only — when a command's contract changes this
# breaks loudly instead of drifting into a lie.
#
# It also configures a real [dev_server] (a `python3 -m http.server` on
# ports 8700+), so `wt dev start/stop`, port allocation, the row's URL and
# the browser-tab cleanup hanging off the port can all be exercised for
# real without touching a project's dev command.
#
# Known gaps (things a local fixture can't fabricate): PR / check /
# merge-queue badges need GitHub, fold state is TUI-only (press Tab on a
# section header), and a stale work status needs a stale timestamp.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BINARY="${WT_NATIVE_BIN:-$ROOT/target/debug/wt}"
FX="${WT_FIXTURE_ROOT:-${TMPDIR:-/tmp}/wt-fixture}"
FX="${FX%/}"
# Resolve symlinks in the path. macOS's $TMPDIR is /var/... which is a
# symlink to /private/var/..., and `git worktree list` reports physical
# paths — a config whose worktree_root is the symlinked spelling
# discovers zero worktrees and renders an empty, blameless board.
mkdir -p "$(dirname "$FX")"
FX="$(cd "$(dirname "$FX")" && pwd -P)/$(basename "$FX")"
CONFIG="$FX/config.toml"
SOCKET="${WT_FIXTURE_SOCKET:-wt-fixture}"
PROBE="${WT_FIXTURE_PROBE:-fixture}"
PREFIX=fx

die() { echo "ERROR: $*" >&2; exit 1; }

# Run wt against the fixture. Every knob is pinned here rather than
# inherited so an ambient WT_CONFIG in the caller's shell can never
# aim these writes at the live instance.
wtfx() {
  [ -x "$BINARY" ] || die 'Build wt first: cargo build -p wt-app (or set WT_NATIVE_BIN)'
  HOME="$FX/home" WT_CONFIG="$CONFIG" WT_REPO_CONFIG="$CONFIG" WT_TMUX_SOCKET="$SOCKET" \
    WT_GITHUB=off WT_AUTOMATIONS=off WT_UPDATE=off WT_SKILLS=off \
    env -u BUN_INSPECT "$BINARY" "$@"
}

new_wt() { # slug [base-ref]
  local slug="$1" base="${2:-}"
  if [ -n "$base" ]; then
    wtfx new "$slug" --base "$base" --no-open --no-install >/dev/null
  else
    wtfx new "$slug" --no-open --no-install >/dev/null
  fi
  printf '.'
}

commit_in() { # slug subject
  (
    cd "$FX/wts/$1"
    echo "$2" >> notes.md
    git add notes.md
    git commit -qm "$2"
  )
}

land_in_trunk() { # slug — merge the worktree's branch into origin/main
  # Makes the row read `merged`, which is what `isCleanCandidate` keys on
  # and the only way to put a clean-sweep candidate on a GitHub-less
  # board. Merged locally into the main clone, then pushed, so both the
  # containment check and `origin/main` agree.
  #
  # `--no-ff` is load-bearing: a fast-forward leaves the branch tip ON
  # main's first-parent chain, which `branchIsMerged` deliberately reads
  # as "an older main SHA, not landed work". A merge commit attaches the
  # branch as a second parent, which is the shape GitHub produces.
  (
    cd "$FX/main-clone"
    git fetch -q "$FX/wts/$1" HEAD
    git merge -q --no-ff --no-edit FETCH_HEAD -m "Merge $1"
    git push -q origin main
  )
}

check_owned_directory() {
  # Refuse to rm -rf a path that isn't ours. "Ours" is judged per entry
  # rather than by origin.git alone, because the corpse of a torn-down
  # fixture is a real state: a wt process outliving the last `rm`
  # recreates `cache/locks`, and a guard keyed on origin.git then refuses
  # to rebuild over a directory it created itself.
  if [ -d "$FX" ]; then
    for entry in "$FX"/* "$FX"/.[!.]*; do
      [ -e "$entry" ] || continue
      case "${entry##*/}" in
        # *.log: markers the fixture's own teardown hooks append to
        # (destroy_command, [dev_server] stop_command) — written by a
        # previous run of this fixture, so they are ours to delete.
        origin.git|main-clone|wts|cache|home|config.toml|env.sh|*.log) ;;
        *) die "$FX holds ${entry##*/}, which no fixture creates — refusing to delete it" ;;
      esac
    done
  fi
}

build() {
  check_owned_directory
  # A probe still attached to the old fixture keeps polling git in a
  # directory this is about to delete, and races the rebuild for index
  # locks. Stop it first.
  bash "$ROOT/scripts/tui-test.sh" stop "$PROBE" >/dev/null 2>&1 || true
  tmux -L "$SOCKET" kill-server >/dev/null 2>&1 || true
  rm -rf "$FX"
  mkdir -p "$FX/cache" "$FX/wts" "$FX/home"

  # A trunk to fork from. `origin` is a local bare repo, so nothing here
  # ever reaches a remote and `origin/main` resolves for real.
  git init -q --bare "$FX/origin.git"
  git clone -q "$FX/origin.git" "$FX/main-clone" 2>/dev/null   # "cloned an empty repository"
  (
    cd "$FX/main-clone"
    git config user.email fixture@example.invalid
    git config user.name "wt fixture"
    git checkout -q -b main 2>/dev/null || true
    echo "wt render fixture" > README.md
    git add README.md
    git commit -qm "Initial commit"
    git push -q origin main
    git branch -q --set-upstream-to=origin/main main
  )

  cat > "$CONFIG" <<TOML
# Generated by scripts/fixture.sh. Throwaway — rebuild, never hand-edit.
[paths]
main_clone    = "$FX/main-clone"
worktree_root = "$FX/wts"
cache_db      = "$FX/cache/cache.sqlite"
cache_root    = "$FX/cache"
state_db      = "$FX/cache/wt.sqlite"
lock_dir      = "$FX/cache/locks"
log_dir       = "$FX/cache/logs"
app_log_dir   = "$FX/cache/app-logs"

[branch]
prefix = "$PREFIX"
base   = "main"

[naming]
auto_rename = false

# A real tracker, for the same reason [dev_server] is real below: the
# issue row hides itself when this section is absent, so without it the
# fixture cannot show the one shape it is most needed for — a slug that
# carries NO id (which is every slug here) wearing an id set by hand
# with \`#\`. A conditional affordance renders nothing when its input is
# missing, which is indistinguishable from broken.
[issue_tracker]
url_template = "https://tracker.invalid/issue/{id}"

[ui]
# "issue" is in the list for the reason given above the [issue_tracker]
# section; "dev" is there because the section below configures a real
# [dev_server]. Without either, the row is hidden and the fixture
# silently stops covering the surface its own comment claims to exercise.
rows = ["branch", "base", "path", "status", "issue", "dev", "git"]

# A real, supervised, per-worktree dev server — enough to exercise
# \`wt dev start/stop\`, the row's URL, port allocation, and the browser-tab
# cleanup that hangs off the allocated port. Ports sit well clear of any
# real project's range so a fixture server can never be mistaken for one.
[dev_server]
command    = "python3 -m http.server {{port}} --bind 127.0.0.1"
port_base  = 8700
port_range = 50
# Small enough to hit by hand: start three dev servers and the third is
# refused with exit 75, which is the whole point of the cap.
max_concurrent = 2
# The stop-time twin of destroy_command. A dev command routinely creates
# things that are not its children (containers above all), so killing
# the session releases nothing — this marker is how the fixture proves
# the hook actually fires.
stop_command = "echo {{slug}} >> $FX/dev-stopped.log"
# The destructive twin, exercised by \`wt dev reset\`: where stop_command
# keeps the environment's state (fast to retake a slot), this drops it.
reset_command = "echo {{slug}} >> $FX/dev-reset.log"
# "Is this environment actually usable?" — a listening port is not
# readiness, and only the project can answer. Toggle it by creating or
# removing \$FX/unhealthy.
health_command = "test ! -f $FX/unhealthy || { echo 'fixture marked unhealthy'; exit 1; }"

[lifecycle]
# Exercises the destroy-time teardown hook. The process reaper can only
# see things holding a listening socket with a cwd in the worktree, so a
# project that creates anything else (docker containers above all) needs
# this to release it. The marker lands OUTSIDE the checkout on purpose —
# the checkout is what's being deleted.
destroy_command = "echo {{slug}} >> $FX/destroyed.log"

[skills]
# Never answer the machine-global skills prompt on the human's behalf.
startup_check = false
TOML

  cat > "$FX/env.sh" <<ENV
# source this to point a shell at the fixture instance
export WT_CONFIG="$CONFIG"
export WT_REPO_CONFIG="$CONFIG"
export HOME="$FX/home"
export WT_TMUX_SOCKET="$SOCKET"
export WT_GITHUB=off WT_AUTOMATIONS=off WT_UPDATE=off WT_SKILLS=off
ENV

  printf 'building fleet '

  # --- Inbox: the baselines every other row is read against ------------
  new_wt plain-task;  commit_in plain-task  "Plain unstacked row"
  new_wt dirty-task;  commit_in dirty-task  "Row with uncommitted work"
  echo "scratch" >> "$FX/wts/dirty-task/notes.md"   # leaves it dirty
  new_wt fresh-task   # no commits: the title falls back to the slug

  # --- Clean-sweep candidates, including the one that must NOT be swept -
  # `c` keys on the BRANCH having landed; whether the checkout still
  # holds uncommitted work is an independent question, and conflating
  # the two once destroyed a rift worktree with 16 uncommitted files in
  # it. Both rows land in trunk; only one is safe to remove.
  new_wt landed-clean; commit_in landed-clean "Landed work, nothing left behind"
  land_in_trunk landed-clean
  new_wt landed-dirty; commit_in landed-dirty "Landed work, then kept going"
  land_in_trunk landed-dirty
  echo "uncommitted follow-up" >> "$FX/wts/landed-dirty/notes.md"
  echo "new file nobody committed" > "$FX/wts/landed-dirty/scratch.md"
  # The third landed shape, and the only one where LANDING is what makes
  # the row loud: a merged branch still owing a check that could not run
  # until it deployed. It renders the work dot instead of the merge
  # glyph, sorts as needs-testing instead of sinking, and is kept by the
  # sweep — none of which any other row on a local board can show.
  new_wt landed-unverified
  commit_in landed-unverified "Landed work whose proof lives in staging"
  land_in_trunk landed-unverified
  wtfx section mv landed-clean landed-dirty landed-unverified "Landed" >/dev/null
  wtfx status landed-unverified ready --risk low \
    --verify-after-merge "sign in with Google on staging, revoke the grant, confirm the reconnect prompt" \
    -m "Sample landed-unverified note.
OPS:      none
REVERT:   safe
IF WRONG: nothing real is behind this row" >/dev/null

  # --- Every work status, so the dot column shows its whole palette ----
  for s in todo working review needs-testing needs-human ready-low ready-high blocked; do
    new_wt "st-$s"
    commit_in "st-$s" "Status sample: $s"
  done
  wtfx section mv st-todo st-working st-review st-needs-testing \
    st-needs-human st-ready-low st-ready-high st-blocked "Statuses" >/dev/null
  wtfx status st-todo todo >/dev/null
  wtfx status st-working working >/dev/null
  wtfx status st-review review >/dev/null
  wtfx status st-needs-testing needs-testing >/dev/null
  wtfx status st-needs-human needs-human \
    -m "blocked on a 2FA prompt; tried the keychain path and the scripted login" >/dev/null
  wtfx status st-ready-low ready --risk low \
    -m "Sample ready note.
OPS:      none
REVERT:   safe
IF WRONG: nothing real is behind this row" >/dev/null
  wtfx status st-ready-high ready --risk high \
    -m "Sample high-risk note.
OPS:      none
REVERT:   no: fixture rows are not revertable, they are fabricated
IF WRONG: nothing real is behind this row
UNTESTED: everything, by construction" >/dev/null
  # A `ready` held back by an external gate: the one record whose STATE
  # and RENDER deliberately disagree (warn circle-slash, out of the merge
  # band, `blocked - ready` in the banner). Without a row carrying it,
  # the whole rendering branch is invisible on any local board.
  wtfx status st-blocked ready --risk low \
    --blocked-on "mobile 2.14 shipped and old builds drained" \
    -m "Sample gated note.
OPS:      none
REVERT:   safe
IF WRONG: nothing real is behind this row" >/dev/null

  # --- A chain: depth 0/1/2, co-located, so the rail has somewhere to go
  new_wt chain-root;                            commit_in chain-root "Chain root lays the schema"
  new_wt chain-mid  "$PREFIX/chain-root";       commit_in chain-mid  "Chain middle adds the reader"
  new_wt chain-leaf "$PREFIX/chain-mid";        commit_in chain-leaf "Chain leaf wires the cache"
  # Deliberately never committed: a stacked worktree between `wt new` and
  # its first commit shares its parent's tip, which is the population the
  # vacuous-containment guard protects (an empty branch is "not started",
  # never "merged"). Its title falls back to the slug for the same reason.
  new_wt chain-unstarted "$PREFIX/chain-root"
  wtfx section mv chain-root "Chain" >/dev/null   # a stack moves as a unit

  # --- A fan: two siblings off one parent, no order between them -------
  new_wt fan-root;                              commit_in fan-root  "Fan root extracts the client"
  new_wt fan-left  "$PREFIX/fan-root";          commit_in fan-left  "Fan left adds the writer"
  new_wt fan-right "$PREFIX/fan-root";          commit_in fan-right "Fan right adds the reporter"
  wtfx section mv fan-root "Fan" >/dev/null

  # --- A split stack: parent filed away from its children --------------
  # The children draw no rail (it would point at a row that isn't above
  # them) and carry a reference to the section their parent went to.
  new_wt split-parent;                          commit_in split-parent  "Split parent finishes the gate"
  new_wt split-kid-a "$PREFIX/split-parent";    commit_in split-kid-a   "Split child A scopes the brand"
  new_wt split-kid-b "$PREFIX/split-parent";    commit_in split-kid-b   "Split child B retries the lookup"
  wtfx section mv split-parent "Hold: Verify on Dev" --only >/dev/null
  echo " done"

  cat <<'BOARD'

board:
  Inbox                 plain / dirty / no-commit rows, plus the two split
                        children (no rail, "-> Hold: Verify on Dev" reference)
  Landed                three merged rows = the `c` sweep's candidate set;
                        landed-dirty holds uncommitted work and
                        landed-unverified still owes a post-merge check,
                        so both must be KEPT by the sweep, not destroyed
  Statuses              one row per work state, ready at low and high risk
  Chain                 depth 0/1/2 co-located: rail steps right per level,
                        plus a stacked row with zero commits of its own
  Fan                   two siblings sharing depth 1: no implied merge order
  Hold: Verify on Dev   the split parent its two inbox children point at

can't be fabricated locally: PR / check / merge-queue badges (need GitHub),
fold state (press Tab on a section header), stale work statuses (need age).
BOARD
  echo
  echo "probe it:  scripts/fixture.sh probe"
  echo "drive it:  scripts/fixture.sh wt ls"
}

cmd="${1:-build}"
shift || true

case "$cmd" in
  build)
    build
    ;;
  probe)
    name="${1:-$PROBE}" w="${2:-200}" h="${3:-50}"
    [ -f "$CONFIG" ] || build
    HOME="$FX/home" WT_CONFIG="$CONFIG" WT_REPO_CONFIG="$CONFIG" WT_TMUX_SOCKET="$SOCKET" WT_NATIVE_BIN="$BINARY" \
      bash "$ROOT/scripts/tui-test.sh" start "$name" "$w" "$h"
    echo "snap:  scripts/tui-test.sh snap $name"
    echo "keys:  scripts/tui-test.sh keys $name j j Tab   (anything goes — it's sealed)"
    ;;
  wt)
    [ -f "$CONFIG" ] || die "no fixture yet — run: scripts/fixture.sh build"
    wtfx "$@"
    ;;
  env)
    [ -f "$FX/env.sh" ] || die "no fixture yet — run: scripts/fixture.sh build"
    echo "$FX/env.sh"
    ;;
  rm)
    check_owned_directory
    bash "$ROOT/scripts/tui-test.sh" stop "$PROBE" >/dev/null 2>&1 || true
    tmux -L "$SOCKET" kill-server >/dev/null 2>&1 || true
    rm -rf "$FX"
    echo "removed $FX"
    ;;
  *)
    sed -n '2,30p' "$0"
    exit 2
    ;;
esac
