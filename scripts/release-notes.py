#!/usr/bin/env python3
"""Write a release's title and notes from every commit since the previous release.

A stable release covers all commits after the previous stable tag, not only
its release commit. With OPENAI_API_KEY set the commit messages are
summarized by a model; without it, or when that call or its answer fails any
check, the notes are assembled from the commit messages themselves. A
prerelease gets no summary. Commit messages of this public repository are the
only text sent.
"""
from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
from pathlib import Path
from urllib.error import HTTPError, URLError
from urllib.request import Request, urlopen

USER_AGENT = "OpenAI File Downloader, XaiImageApiFetch/1.0"
STABLE_TAG = re.compile(r"v\d+\.\d+\.\d+")
API = "https://api.openai.com/v1"
# Discord shows the notes in an embed capped at 4096 characters.
MAX_NOTES = 3500
MAX_TITLE = 72
MAX_COMMIT_TEXT = 60_000
FALLBACK_MODEL = "gpt-4o"

INSTRUCTIONS = """You write release notes for wt, a terminal application and CLI for managing Git \
worktrees and the AI agent sessions working in them. You are given every commit message since \
the previous release. Write for a person who uses wt, not for its developers.

Reply with one JSON object: {"title": string, "notes": string}.

title: what this release is mainly about, as a short lower-case phrase list of at most 60 \
characters, such as "menus from the board, row-granular reads, faster sessions". No version \
number, no trailing period.

notes: GitHub-flavoured Markdown of at most 2800 characters. Start with one or two plain \
sentences on what the release changes for the person using it. Then bullet lists under bold \
headings chosen from **Added**, **Faster**, **Changed** and **Fixed**, in that order, leaving \
out any that would be empty. Each bullet is one user-visible change in one or two sentences: \
what happens now, in terms of commands, keys and screens. Merge commits that refine the same \
change into one bullet, and say only the final behaviour. Leave out internal refactors, tests, \
review rounds, CI and tooling unless they change what a user sees.

Rules: use only what the commit messages say and invent nothing. Plain technical English, \
present tense. No sales language, no exclamation marks, no emoji, no em dashes. Put command \
names, flags, keys and config keys in backticks. No commit hashes, no author names, no links."""


def git(*args: str) -> str:
    return subprocess.run(["git", *args], check=True, capture_output=True, text=True).stdout


def previous_release(build: str) -> str | None:
    """The newest stable tag among the ancestors of the release commit."""
    try:
        tags = git("tag", "--merged", f"{build}^", "--sort=-v:refname").split()
    except subprocess.CalledProcessError:
        return None
    return next((tag for tag in tags if STABLE_TAG.fullmatch(tag)), None)


def commits_since(previous: str | None, build: str) -> list[dict]:
    """Subject and body of each commit of the release, oldest first."""
    span = [f"{previous}..{build}"] if previous else ["-n", "50", build]
    raw = git("log", "--reverse", "--no-merges", "--format=%s%x1f%b%x1e", *span)
    commits = []
    for record in raw.split("\x1e"):
        subject, _, body = record.strip("\n").partition("\x1f")
        if subject.strip():
            commits.append({"subject": subject.strip(), "body": body.strip()})
    return commits


def strip_release_prefix(subject: str, release: str) -> str:
    return re.sub(rf"^Release {re.escape(release)}:\s*", "", subject).strip()


def assembled(release: str, previous: str | None, commits: list[dict]) -> dict:
    """Notes built from the commit messages alone: the release commit's own
    summary, then the subject of every other commit it ships."""
    headline, summary, others = "", "", []
    for commit in commits:
        subject = commit["subject"]
        if subject.startswith(f"Release {release}:"):
            headline = strip_release_prefix(subject, release)
            summary = commit["body"]
        elif not subject.startswith("Release v"):
            others.append(subject)
    if not headline and others:
        headline = others[-1]
    parts = [part for part in (headline, summary) if part]
    if others:
        since = f" since {previous}" if previous else ""
        parts.append(f"**Changes{since}**\n" + "\n".join(f"- {subject}" for subject in others))
    title = headline if len(headline) <= MAX_TITLE else ""
    return {"title": calm(title), "notes": clip(calm("\n\n".join(parts)))}


def clip(notes: str) -> str:
    if len(notes) <= MAX_NOTES:
        return notes
    kept = notes[:MAX_NOTES].rsplit("\n", 1)[0].rstrip()
    return f"{kept}\n- and more: see the commit history"


def request(url: str, key: str, payload: dict | None = None) -> dict:
    headers = {"User-Agent": USER_AGENT, "Authorization": f"Bearer {key}"}
    body = None
    if payload is not None:
        headers["Content-Type"] = "application/json"
        body = json.dumps(payload).encode()
    try:
        with urlopen(Request(url, data=body, headers=headers), timeout=120) as response:
            return json.loads(response.read(8 * 1024 * 1024))
    except HTTPError as error:
        # The body may quote the request; say only what failed.
        raise RuntimeError(f"HTTP {error.code} from {url.removeprefix(API)}") from None
    except (URLError, TimeoutError, ValueError) as error:
        raise RuntimeError(f"{type(error).__name__} from {url.removeprefix(API)}") from None


def pick_model(listing: dict) -> str:
    """The newest general model the key can use: the highest plain gpt-N or
    gpt-N.M, so a retired default cannot break a release years on."""
    best = None
    for entry in listing.get("data", []):
        match = re.fullmatch(r"gpt-(\d+)(?:\.(\d+))?", str(entry.get("id", "")))
        if match:
            version = (int(match[1]), int(match[2] or 0))
            if best is None or version > best[0]:
                best = (version, entry["id"])
    return best[1] if best else FALLBACK_MODEL


RULE = re.compile(r"(?m)^[ \t]*(-{3,}|\*{3,}|_{3,})[ \t]*$")


def calm(text: str) -> str:
    """Text that notifies nobody and cannot end the notes early: an @name in a
    GitHub release body notifies that account, and a rule line is where the
    publisher and the Discord post cut the notes from the footer."""
    return RULE.sub("", re.sub(r"@(?=\w)", "@" + chr(0x200B), text))


def checked(answer: object) -> dict:
    """The model's answer, or an error naming what is wrong with it."""
    if not isinstance(answer, dict):
        raise ValueError("answer is not an object")
    title, notes = answer.get("title"), answer.get("notes")
    if not isinstance(title, str) or not isinstance(notes, str):
        raise ValueError("title or notes is not text")
    title = re.sub(r"\s+", " ", title.replace("—", ", ")).strip().rstrip(".")
    notes = notes.replace("—", ", ").strip()
    if not title or len(title) > MAX_TITLE:
        raise ValueError("title is empty or too long")
    if len(notes) < 80 or len(notes) > MAX_NOTES:
        raise ValueError("notes are too short or too long")
    if RULE.search(notes):
        # The publisher separates the notes from its footer with a rule.
        raise ValueError("notes contain a horizontal rule")
    if re.search(r"https?://|\]\(", notes):
        raise ValueError("notes contain a link")
    # A heading needs a blank line before it, or the Discord post joins it
    # to the sentence above.
    notes = re.sub(r"(?m)(?<=[^\n])\n(\*\*[^*\n]+\*\*[ \t]*)$", r"\n\n\1", notes)
    return {"title": calm(title), "notes": calm(notes)}


def summarized(release: str, previous: str | None, commits: list[dict], env, transport=request) -> dict:
    key = env["OPENAI_API_KEY"]
    model = env.get("RELEASE_NOTES_MODEL") or pick_model(transport(f"{API}/models", key))
    log = "\n\n".join(f"{commit['subject']}\n{commit['body']}".strip() for commit in commits)
    span = f"{previous} to {release}" if previous else release
    reply = transport(f"{API}/chat/completions", key, {
        "model": model,
        "response_format": {"type": "json_object"},
        "messages": [
            {"role": "system", "content": INSTRUCTIONS},
            # The newest commits, and the release commit's own summary, are last.
            {"role": "user", "content": f"Commits from {span}, oldest first:\n\n{log[-MAX_COMMIT_TEXT:]}"},
        ],
    })
    result = checked(json.loads(reply["choices"][0]["message"]["content"]))
    print(f"release notes summarized by {model}", file=sys.stderr)
    return result


def release_notes(release: str, build: str, env, transport=request) -> dict:
    """The notes a stable release is published with; a prerelease has none."""
    if not STABLE_TAG.fullmatch(release):
        return {"title": "", "notes": ""}
    return drafted(release, build, env, transport)


def drafted(release: str, build: str, env, transport=request) -> dict:
    """Notes for everything since the previous stable tag, whatever `release`
    is: a dispatch that publishes nothing shows them as a draft."""
    previous = previous_release(build)
    commits = commits_since(previous, build)
    if env.get("OPENAI_API_KEY") and commits:
        # Notes never stand between a checked build and its release: whatever
        # the service or its answer does, the commit messages are used.
        try:
            return summarized(release, previous, commits, env, transport)
        except Exception as error:  # noqa: BLE001
            print(f"release notes: summary unavailable ({type(error).__name__}: {error}); "
                  "using commit messages", file=sys.stderr)
    return assembled(release, previous, commits)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--release", required=True)
    parser.add_argument("--build", required=True)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--summary", type=Path)
    args = parser.parse_args()
    stable = bool(STABLE_TAG.fullmatch(args.release))
    empty = {"title": "", "notes": ""}
    # A preview of main shows the draft for the next release; a test tag has
    # nothing to preview and spends no request.
    shown = empty
    if stable or args.release.startswith("preview-"):
        try:
            shown = drafted(args.release, args.build, os.environ)
        except Exception as error:  # noqa: BLE001
            print(f"release notes: could not be written ({type(error).__name__}); publishing without",
                  file=sys.stderr)
    result = shown if stable else empty
    args.output.write_text(json.dumps(result, indent=2) + "\n")
    if args.summary:
        heading = f"wt {args.release}" if stable else "Draft notes for the next stable release"
        if shown["title"]:
            heading += f": {shown['title']}"
        if shown["notes"]:
            with args.summary.open("a") as summary:
                summary.write(f"## {heading}\n\n{shown['notes']}\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
