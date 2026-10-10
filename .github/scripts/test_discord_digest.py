import contextlib
import importlib.util
import io
import json
from pathlib import Path
import unittest

spec = importlib.util.spec_from_file_location("digest", Path(__file__).with_name("discord-digest.py"))
digest = importlib.util.module_from_spec(spec)
spec.loader.exec_module(digest)


def commit(title, author="someone", parents=1):
    return {"parents": [{}] * parents, "author": {"login": author},
            "commit": {"message": title, "author": {"name": author}}}


class DigestTests(unittest.TestCase):
    def env(self, **extra):
        return {"GITHUB_REPOSITORY": "fixture/wt", "HEAD_SHA": "new",
                "GITHUB_TOKEN": "fixture-token", **extra}

    def test_same_head_performs_no_summary_or_webhook_write(self):
        calls = []
        def transport(url, **kwargs):
            calls.append((url, kwargs))
            return {"workflow_runs": [{"head_sha": "new"}]}
        with contextlib.redirect_stdout(io.StringIO()):
            digest.Digest(self.env(OPENAI_API_KEY="fixture"), transport).run()
        self.assertEqual(len(calls), 1)
        self.assertNotIn("payload", calls[0][1])

    def test_dry_run_bounds_commits_filters_merges_and_does_not_post(self):
        calls = []
        def transport(url, **kwargs):
            calls.append((url, kwargs))
            if "/runs?" in url:
                return {"workflow_runs": [{"head_sha": "old"}]}
            if "/compare/" in url:
                return {"html_url": "https://github.com/fixture/wt/compare/old...new",
                        "commits": [commit(str(n)) for n in range(40)] + [commit("merge", parents=2)]}
            self.fail("dry run made an unexpected request")
        out = io.StringIO()
        with contextlib.redirect_stdout(out), contextlib.redirect_stderr(io.StringIO()):
            digest.Digest(self.env(DRY_RUN="1"), transport).run()
        embed = json.loads(out.getvalue())["embeds"][0]
        self.assertEqual(embed["description"].splitlines()[0], "- 10")
        self.assertEqual(embed["description"].splitlines()[-1], "- 39")
        self.assertEqual(embed["footer"]["text"], "30 commits by @someone")
        self.assertEqual(len(calls), 2)

    def test_failed_summary_posts_titles_once_and_failed_post_propagates(self):
        writes = []
        def transport(url, **kwargs):
            if "/runs?" in url:
                return {"workflow_runs": []}
            if "/commits?" in url:
                return [commit("newer"), commit("older")]
            if "api.openai.com" in url:
                raise RuntimeError("fixture model failure")
            writes.append(kwargs["payload"])
            raise RuntimeError("fixture webhook failure")
        with contextlib.redirect_stderr(io.StringIO()):
            with self.assertRaisesRegex(RuntimeError, "fixture webhook failure"):
                digest.Digest(self.env(OPENAI_API_KEY="fixture", DISCORD_WEBHOOK="fixture-webhook"), transport).run()
        self.assertEqual(len(writes), 1)
        self.assertEqual(writes[0]["embeds"][0]["description"], "- older\n- newer")

    def test_notes_and_authors_keep_order_without_duplicate_authors(self):
        commits = [commit("First\n\nDetails\nMore", "alex"), commit("Second", "michael"), commit("Third", "alex")]
        self.assertEqual(digest.authors_line(commits), "@alex and @michael")
        self.assertEqual(digest.commit_notes(commits), "- First\n  Details\n  More\n- Second\n- Third")
        self.assertLessEqual(len(digest.commit_notes([commit("Title\n" + "x" * 1000)])), 710)


if __name__ == "__main__":
    unittest.main()
