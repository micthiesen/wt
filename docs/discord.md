# Discord server wiring

How the wt Discord server (invite: <https://discord.gg/DDnxyXQgF7>, never
expires) is connected to this repo. Server id: `1534621499665813627` (widget
enabled — that id is public by design; it powers the README badge).

## Channels

- **#general** — chat.
- **#updates** — AI-written commit digests, posted by the workflow below.
  Nothing else posts here.
- **#github** — raw repo events via a native Discord GitHub webhook (see
  below), plus unsuccessful `ci` completions on `main`. Deliberately excludes
  pushes so it never overlaps #updates.
- **#help** — support. Carries a pinned message telling reporters to include
  `wt doctor` output and the tail of `~/.cache/wt/logs/app/wt-<date>.log`.

## #updates: the commit digest

`.github/workflows/discord-digest.yml` + `.github/scripts/discord-digest.ts`.
Full mechanics are documented in those files' header comments; the shape:

- Triggers via `workflow_run` when the `ci` workflow goes green on `main` (a
  red tree is never digested; its commits roll into the next green digest).
  The trigger names the workflow by its `name:` — if `ci` is ever renamed,
  update `workflows: [ci]` here in the same commit, or digests silently stop
  (this bit us once when `typecheck` became `ci`).
- Debounce: the run sleeps 30 minutes inside a `concurrency` group with
  `cancel-in-progress: true`, so every newer green push cancels the sleeping
  run and restarts the countdown — one digest per burst. The group sits at
  the JOB level, not the workflow level: a workflow-level group is claimed
  even by runs whose job the green-only `if:` skips, so a red push was
  cancelling the previous green push's pending digest (its commits then
  silently waited for the next green burst). The idle runner is
  free on public repos; if this repo ever goes private, switch to a cron
  check instead.
- "Since when" state is the head SHA of the digest workflow's own last
  successful run — no tags, no state files.
- The model (`gpt-5.6-luna`) sees commit titles + bodies only, never diffs.
  OpenAI failure degrades to posting raw commit titles.
- Attribution is built in: the embed footer lists commit authors' GitHub
  logins.
- **Silence is ambiguous, and #updates is the only place it shows.** Because
  the trigger is green-`ci`-on-`main`, "no digest for two days" has two
  readings that look identical from the channel: nobody pushed, or `main` has
  been red and every commit is queued for the next green digest. It was the
  second on 2026-08-24 to 08-26 (eight pushes, four config-leaking
  `rollupChecklist` tests, fixed in `test/preload.ts` + `test/config.toml`),
  and nothing anywhere announced it — the same green gate also holds back
  hot updates, so users had been getting nothing either. Before treating a
  quiet #updates as a digest bug, check `gh run list --workflow=ci.yml
  --branch=main`; a run of `skipped` digest runs is the tell, since the
  green-only `if:` skips rather than fails.
- A GitHub Actions outage can wedge a `push`-triggered ci run in
  `queued` indefinitely — `gh run cancel`/`rerun` both error unhelpfully on
  it ("already completed" / "already running") rather than clearing it. The
  stuck run itself has no fix; a new push supersedes it and the debounce
  resumes normally once ci runs are getting picked up again.

**Actions secrets** (repo Settings → Secrets → Actions): `OPENAI_API_KEY`,
`DISCORD_UPDATES_WEBHOOK` (the #updates channel webhook URL).

## #github: repo events and CI failures

A GitHub **repo webhook** (repo Settings → Webhooks — not an Actions secret)
posting to the #github channel's Discord webhook URL with `/github` appended.
Registered events: `issues`, `issue_comment`, `pull_request`,
`pull_request_review`, `fork`, `star` — no `push`. Hook id `662175381`.

`.github/workflows/discord-ci-alert.yml` separately posts every non-successful
completion of the `ci` workflow on `main`, including failures, cancellations,
and timeouts. The alert includes the conclusion, commit, actor, and a direct
link to the Actions run. It uses the Actions secret `DISCORD_GITHUB_WEBHOOK`,
which is the same channel webhook URL without the native integration's
`/github` suffix.

To re-register (e.g. after rotating the Discord webhook):

```sh
gh api repos/micthiesen/wt/hooks -X POST -F active=true -f name=web \
  -f 'events[]=issues' -f 'events[]=issue_comment' -f 'events[]=pull_request' \
  -f 'events[]=pull_request_review' -f 'events[]=fork' -f 'events[]=star' \
  -f 'config[url]=<discord-webhook-url>/github' -f 'config[content_type]=json'
```

Discord webhook URLs grant post access to the channel — they live only in
Discord (channel → Integrations → Webhooks), repo webhook settings, and Actions
secrets; never commit one.

## Repo surfaces pointing at the server

- README badge: live online count via
  `img.shields.io/discord/<server-id>` (requires the server widget to stay
  enabled), linking the invite.
- README **Community** section.
- Repo homepage (About field) is the invite URL.
- `.github/ISSUE_TEMPLATE/config.yml` offers the Discord as a contact link on
  the new-issue page.
- Logo: `docs/logo.svg` (editable source) → `docs/logo.png` (512px, rounded,
  transparent corners; rasterize via a Chromium `--headless=new --screenshot`
  with `--default-background-color=00000000` — macOS Quick Look flattens SVG
  transparency to white). The square, uncropped variant of the same art is
  the Discord server icon.

## Deliberately not set up (revisit when the server grows)

- Release flow / release announcements — too heavy for now; the digest covers
  it.
- Forum-style #help (needs Community mode) and an opt-in @updates ping role —
  wait for strangers.
- Stats/moderation bots, server banner, vanity URL.
