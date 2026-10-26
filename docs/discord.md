# Discord server wiring

How the wt Discord server (invite: <https://discord.gg/DDnxyXQgF7>, never
expires) is connected to this repo. Server id: `1534621499665813627` (widget
enabled — that id is public by design; it powers the README badge).

## Channels

- **#general** — chat.
- **#updates** — one post per published stable release, with its notes.
  Nothing else posts here.
- **#github** — raw repo events via a native Discord GitHub webhook (see
  below), plus unsuccessful, non-cancelled `Native release` completions on `main`. Deliberately excludes
  pushes so it never overlaps #updates.
- **#help** — support. Carries a pinned message telling reporters to include
  `wt doctor` output and the tail of `~/.cache/wt/logs/app/wt-native.<date>.log`.

## #updates: release announcements

`.github/workflows/discord-release.yml` + `.github/scripts/discord-release.py`
(standard-library Python, no JavaScript runtime). Each published stable release
is posted once, as an embed titled `wt vX.Y.Z` that links the GitHub release and
carries its notes.

- The notes are the release commit's message: `scripts/publish-native.py`
  writes its summary line (without the `Release vX.Y.Z:` prefix) and body into
  stable GitHub releases above the download boilerplate, and the post drops that
  boilerplate. Preview (`preview-<sha>`) releases keep only the boilerplate and
  are never posted.
- `publish-native.py` creates releases with `GITHUB_TOKEN`, which cannot
  trigger `release` events, so the workflow follows `Native release` through
  `workflow_run`. It runs only for a successful `push` run whose ref is a `v*`
  tag. The script then requires a stable, non-draft release for that tag
  published from the run's commit, so the `main` run of the same commit posts
  nothing.
- The trigger names the workflow by its `name:`. If `Native release` is renamed,
  update both Discord workflows in the same commit, or posts silently stop.
- A failed post does not affect the release. Re-run the `discord-release` run to
  retry it.
- **Silence means no stable release was published.** A failed release run posts
  nothing here; its failure shows in #github.

This replaced an AI-written digest of commits on `main`, which predates the
versioned release flow. The `OPENAI_API_KEY` Actions secret it used is no
longer read.

**Actions secret** (repo Settings → Secrets → Actions):
`DISCORD_UPDATES_WEBHOOK` (the #updates channel webhook URL).

## #github: repo events and CI failures

A GitHub **repo webhook** (repo Settings → Webhooks — not an Actions secret)
posting to the #github channel's Discord webhook URL with `/github` appended.
Registered events: `issues`, `fork`, `star` — no `push` or pull-request
events. `issue_comment` is deliberately excluded because GitHub also uses it
for pull-request comments. Hook id `662175381`.

`.github/workflows/discord-ci-alert.yml` separately posts unsuccessful
completions of `Native release` on `main`, including failures and timeouts.
Cancelled runs are silent. The alert includes the conclusion, commit, actor, and a direct
link to the Actions run. It uses the Actions secret `DISCORD_GITHUB_WEBHOOK`,
which is the same channel webhook URL without the native integration's
`/github` suffix.

To re-register (e.g. after rotating the Discord webhook):

```sh
gh api repos/micthiesen/wt/hooks -X POST -F active=true -f name=web \
  -f 'events[]=issues' -f 'events[]=fork' -f 'events[]=star' \
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

- Forum-style #help (needs Community mode) and an opt-in @updates ping role —
  wait for strangers.
- Stats/moderation bots, server banner, vanity URL.
