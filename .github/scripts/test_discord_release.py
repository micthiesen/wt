import importlib.util
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("discord_release", Path(__file__).with_name("discord-release.py"))
announcer = importlib.util.module_from_spec(spec)
spec.loader.exec_module(announcer)

SHA = "a" * 40


def release(**overrides):
    value = {
        "tag_name": "v1.2.3",
        "name": "wt v1.2.3",
        "html_url": "https://github.com/o/r/releases/tag/v1.2.3",
        "body": "Faster CI\n\n- parallel checks\n\n---\nNative wt binaries for macOS and Linux.",
        "published_at": "2026-10-26T18:00:00Z",
        "prerelease": False,
        "draft": False,
        "target_commitish": SHA,
    }
    value.update(overrides)
    return value


class Transport:
    def __init__(self, response):
        self.response = response
        self.calls = []

    def __call__(self, url, *, headers=None, payload=None):
        self.calls.append((url, payload))
        return self.response if payload is None else None


def env(**overrides):
    value = {"RELEASE_TAG": "v1.2.3", "GITHUB_REPOSITORY": "o/r", "GITHUB_TOKEN": "t",
             "HEAD_SHA": SHA, "DISCORD_WEBHOOK": "https://discord.example/hook"}
    value.update(overrides)
    return value


class DiscordReleaseTests(unittest.TestCase):
    def test_posts_stable_release_notes_without_download_boilerplate(self):
        transport = Transport(release())
        self.assertEqual(announcer.run(env(), transport), 0)
        url, posted = transport.calls[-1]
        self.assertEqual(url, "https://discord.example/hook")
        embed = posted["embeds"][0]
        self.assertEqual(embed["title"], "wt v1.2.3")
        self.assertEqual(embed["description"], "Faster CI\n\n- parallel checks")
        self.assertEqual(embed["url"], "https://github.com/o/r/releases/tag/v1.2.3")

    def test_preview_tags_and_prereleases_post_nothing(self):
        for tag in ["main", "preview-" + SHA, "rust-test-x", "v1.2"]:
            transport = Transport(release())
            self.assertEqual(announcer.run(env(RELEASE_TAG=tag), transport), 0)
            self.assertEqual(transport.calls, [])
        transport = Transport(release(prerelease=True))
        announcer.run(env(), transport)
        self.assertEqual(len(transport.calls), 1)

    def test_release_from_another_commit_is_refused(self):
        with self.assertRaises(RuntimeError):
            announcer.run(env(), Transport(release(target_commitish="b" * 40)))

    def test_long_notes_are_truncated_for_discord(self):
        text = announcer.description("x" * 5000)
        self.assertEqual(len(text), announcer.MAX_DESCRIPTION)
        self.assertTrue(text.endswith("…"))


if __name__ == "__main__":
    unittest.main()
