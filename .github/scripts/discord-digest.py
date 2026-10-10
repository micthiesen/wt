#!/usr/bin/env python3
"""Write the Discord update digest from commit metadata, without a JS runtime.

Invoked by discord-digest.yml after debounce. DRY_RUN=1 prints the payload;
OPENAI_API_KEY is optional and failure falls back to commit titles.
"""
from __future__ import annotations

from datetime import datetime, timezone
import json
import os
import sys
from urllib.error import HTTPError
from urllib.request import Request, urlopen

USER_AGENT = "OpenAI File Downloader, XaiImageApiFetch/1.0"
MAX_COMMITS = 30
MAX_BODY_CHARS = 700
SYSTEM_PROMPT = (
    "You write the #updates posts for the Discord server of wt, an open-source "
    "terminal UI for keeping many git worktrees (and their PRs, CI, dev servers, "
    "and coding-agent sessions) in flight at once. Input: the commit titles and "
    "descriptions that landed on main since the last post. Write the update "
    "note: 2-4 plain sentences, or up to 6 short bullet lines when the changes "
    "are unrelated. Lead with what changed for people using wt; fold internal "
    "refactors into a clause or drop them. No hype, no emoji, no headers, no "
    "greeting, no \"this update\". Discord markdown is fine (backticks for "
    "commands/config keys). Stay under 900 characters."
)


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


class Digest:
    def __init__(self, env, transport=request):
        self.env = env
        self.repo = require_env(env, "GITHUB_REPOSITORY")
        self.head = require_env(env, "HEAD_SHA")
        self.token = require_env(env, "GITHUB_TOKEN")
        self.transport = transport

    def gh(self, path):
        return self.transport(
            "https://api.github.com" + path,
            headers={
                "Authorization": "Bearer " + self.token,
                "Accept": "application/vnd.github+json",
                "X-GitHub-Api-Version": "2022-11-28",
            },
        )

    def collect(self):
        since = None
        try:
            runs = self.gh(
                f"/repos/{self.repo}/actions/workflows/discord-digest.yml/runs"
                "?status=success&branch=main&per_page=1"
            )["workflow_runs"]
            since = runs[0]["head_sha"] if runs else None
        except Exception as error:
            print(f"last-run lookup failed: {error}", file=sys.stderr)
        if since == self.head:
            return [], ""
        if since:
            try:
                compared = self.gh(f"/repos/{self.repo}/compare/{since}...{self.head}")
                return compared["commits"], compared["html_url"]
            except Exception as error:
                print(f"compare {since}...{self.head} failed: {error}", file=sys.stderr)
        recent = self.gh(f"/repos/{self.repo}/commits?sha={self.head}&per_page=10")
        return list(reversed(recent)), f"https://github.com/{self.repo}/commits/main"

    def summarize(self, commits):
        fallback = "\n".join("- " + c["commit"]["message"].split("\n")[0] for c in commits)[:3900]
        key = self.env.get("OPENAI_API_KEY")
        if not key:
            print("no OPENAI_API_KEY; posting raw commit titles", file=sys.stderr)
            return fallback
        try:
            data = self.transport(
                "https://api.openai.com/v1/chat/completions",
                headers={"Authorization": "Bearer " + key},
                payload={
                    "model": self.env.get("OPENAI_MODEL") or "gpt-6-luna",
                    "max_completion_tokens": 500,
                    "messages": [
                        {"role": "system", "content": SYSTEM_PROMPT},
                        {"role": "user", "content": commit_notes(commits)},
                    ],
                },
            )
            text = (data["choices"][0]["message"]["content"] or "").strip()
            if not text:
                raise ValueError("empty completion")
            return text[:3900]
        except Exception as error:
            print(f"OpenAI failed, posting raw commit titles: {error}", file=sys.stderr)
            return fallback

    def run(self):
        all_commits, url = self.collect()
        commits = [c for c in all_commits if len(c["parents"]) <= 1][-MAX_COMMITS:]
        if not commits:
            print("no new commits since the last digest; nothing to post")
            return
        count = len(commits)
        payload = {"embeds": [{
            "title": "What's new in wt", "url": url,
            "description": self.summarize(commits), "color": 0x5865F2,
            "footer": {"text": f"{count} commit{'s' if count != 1 else ''} by {authors_line(commits)}"},
            "timestamp": datetime.now(timezone.utc).isoformat().replace("+00:00", "Z"),
        }]}
        if self.env.get("DRY_RUN") == "1":
            print(json.dumps(payload, indent=2))
            return
        self.transport(require_env(self.env, "DISCORD_WEBHOOK"), payload=payload)
        print(f"posted digest of {count} commits")


def commit_notes(commits):
    notes = []
    for commit in commits:
        title, _, body = commit["commit"]["message"].partition("\n")
        body = body.strip()[:MAX_BODY_CHARS]
        notes.append("- " + title + ("\n" + "\n".join("  " + line for line in body.split("\n")) if body else ""))
    return "\n".join(notes)


def authors_line(commits):
    names = list(dict.fromkeys(
        "@" + c["author"]["login"] if (c.get("author") or {}).get("login")
        else (c["commit"].get("author") or {}).get("name", "unknown")
        for c in commits
    ))
    return ", ".join(names[:-1]) + " and " + names[-1] if len(names) > 1 else names[0] if names else "unknown"


if __name__ == "__main__":
    try:
        Digest(os.environ).run()
    except Exception as error:
        print(f"Digest failed: {error}", file=sys.stderr)
        sys.exit(1)
