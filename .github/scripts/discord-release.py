#!/usr/bin/env python3
"""Post a published stable wt release, with its notes, to the Discord #updates channel.

Invoked by discord-release.yml when the native release run for a version tag
succeeds. Only a non-prerelease GitHub release whose tag is that run's ref is
announced, so the main-branch run of the same commit posts nothing.
DRY_RUN=1 prints the payload instead of posting.
"""
from __future__ import annotations

import json
import os
import re
import sys
from urllib.error import HTTPError
from urllib.parse import quote
from urllib.request import Request, urlopen

USER_AGENT = "OpenAI File Downloader, XaiImageApiFetch/1.0"
# Discord embed descriptions are capped at 4096 characters.
MAX_DESCRIPTION = 4000
STABLE_TAG = re.compile(r"v\d+\.\d+\.\d+")


def require_env(env, name):
    value = env.get(name)
    if not value:
        raise ValueError(f"missing env: {name}")
    return value


def request(url, *, headers=None, payload=None):
    headers = {"User-Agent": USER_AGENT, **(headers or {})}
    body = None
    if payload is not None:
        headers["Content-Type"] = "application/json"
        body = json.dumps(payload).encode()
    req = Request(url, data=body, headers=headers)
    try:
        with urlopen(req, timeout=30) as response:
            data = response.read(8 * 1024 * 1024 + 1)
    except HTTPError as error:
        # Do not print the request URL: Discord webhook URLs contain a secret.
        raise RuntimeError(f"HTTP {error.code}: {error.read(1024).decode(errors='replace')}") from None
    if len(data) > 8 * 1024 * 1024:
        raise RuntimeError("response exceeds 8 MiB")
    return json.loads(data) if data else None


def unwrap(text: str) -> str:
    """Join the hard-wrapped continuation lines of a commit message, which
    Discord would otherwise show as breaks mid-sentence."""
    lines: list[str] = []
    for line in text.splitlines():
        continues = (lines and lines[-1].strip() and line.strip()
                     and not line.lstrip().startswith(("- ", "* ")))
        if continues:
            lines[-1] = f"{lines[-1].rstrip()} {line.strip()}"
        else:
            lines.append(line.rstrip())
    return "\n".join(lines)


def description(body: str) -> str:
    """The release notes, without the trailing download boilerplate."""
    notes = unwrap(body.split("\n---\n", 1)[0].strip() or body.strip())
    if len(notes) > MAX_DESCRIPTION:
        notes = notes[: MAX_DESCRIPTION - 1].rstrip() + "…"
    return notes


def payload(release: dict) -> dict:
    return {
        "username": "wt releases",
        "embeds": [{
            "title": release["name"] or f"wt {release['tag_name']}",
            "url": release["html_url"],
            "description": description(release.get("body") or ""),
            "color": 0x5865F2,
            "timestamp": release["published_at"],
        }],
    }


def run(env, transport=request) -> int:
    tag = require_env(env, "RELEASE_TAG")
    if not STABLE_TAG.fullmatch(tag):
        print(f"{tag} is not a stable version tag; nothing to post")
        return 0
    repo = require_env(env, "GITHUB_REPOSITORY")
    release = transport(
        f"https://api.github.com/repos/{repo}/releases/tags/{quote(tag, safe='')}",
        headers={
            "Authorization": "Bearer " + require_env(env, "GITHUB_TOKEN"),
            "Accept": "application/vnd.github+json",
            "X-GitHub-Api-Version": "2022-11-28",
        },
    )
    if release.get("draft") or release.get("prerelease"):
        print(f"{tag} is a draft or prerelease; nothing to post")
        return 0
    if release.get("target_commitish") != require_env(env, "HEAD_SHA"):
        raise RuntimeError(f"{tag} was not published from this run's commit")
    body = payload(release)
    if env.get("DRY_RUN") == "1":
        print(json.dumps(body, indent=2))
        return 0
    transport(require_env(env, "DISCORD_WEBHOOK"), payload=body)
    print(f"posted {tag}")
    return 0


if __name__ == "__main__":
    try:
        sys.exit(run(os.environ))
    except (RuntimeError, ValueError) as error:
        print(f"error: {error}", file=sys.stderr)
        sys.exit(1)
