#!/usr/bin/env python3
"""Release notes cover every commit since the previous stable tag."""
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("notes", Path(__file__).with_name("release-notes.py"))
notes = importlib.util.module_from_spec(spec)
spec.loader.exec_module(notes)

GOOD = {"title": "faster menus, kept gates",
        "notes": "Menus open at once.\n\n**Fixed**\n- A gate survives a status change, and `wt status` says so."}


class ReleaseNotes(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="wt-notes-test-")
        self.addCleanup(self.directory.cleanup)
        self.addCleanup(os.chdir, os.getcwd())
        os.chdir(self.directory.name)
        self.git("init", "-q", "-b", "main")
        self.commit("Start")
        self.git("tag", "v0.1.0")
        self.commit("Old fix", "Detail of the old fix.")
        self.commit("Release v0.1.1: old things")
        self.git("tag", "v0.1.1")
        self.git("tag", "rust-test-abc")
        self.commit("Open menus from the board", "The palette needs no host round trip.")
        self.commit("Keep the gate across status changes")
        self.commit("Release v0.1.2: menus and gates", "- menus open at the keypress\n- gates survive")
        self.commit("Fix the Linux routing")
        self.build = self.git("rev-parse", "HEAD").strip()

    def git(self, *args):
        env = {**os.environ, "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_SYSTEM": os.devnull,
               "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@example.invalid",
               "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@example.invalid"}
        return subprocess.run(["git", *args], check=True, capture_output=True, text=True, env=env).stdout

    def commit(self, subject, body=""):
        self.git("commit", "-q", "--allow-empty", "-m", subject, *(["-m", body] if body else []))

    def test_the_span_is_every_commit_after_the_previous_stable_tag(self):
        self.assertEqual(notes.previous_release(self.build), "v0.1.1")
        subjects = [commit["subject"] for commit in notes.commits_since("v0.1.1", self.build)]
        self.assertEqual(subjects, ["Open menus from the board", "Keep the gate across status changes",
                                    "Release v0.1.2: menus and gates", "Fix the Linux routing"])
        # The first release has no earlier tag to start from.
        first = self.git("rev-parse", "v0.1.0").strip()
        self.assertIsNone(notes.previous_release(first))

    def test_without_a_key_the_notes_are_assembled_from_all_of_the_commits(self):
        result = notes.release_notes("v0.1.2", self.build, {})
        self.assertEqual(result["title"], "menus and gates")
        text = result["notes"]
        self.assertTrue(text.startswith("menus and gates\n\n- menus open at the keypress"), text)
        self.assertIn("**Changes since v0.1.1**", text)
        for subject in ("Open menus from the board", "Keep the gate across status changes",
                        "Fix the Linux routing"):
            self.assertIn(f"- {subject}", text)
        # Nothing from before the previous release, and no release commit as a change.
        self.assertNotIn("Old fix", text)
        self.assertNotIn("- Release v0.1.2", text)

    def test_a_summary_is_asked_of_the_newest_plain_model_with_every_commit(self):
        calls = []

        def transport(url, key, payload=None):
            calls.append((url, key, payload))
            if url.endswith("/models"):
                return {"data": [{"id": "gpt-4o"}, {"id": "gpt-5.2"}, {"id": "gpt-5.10"},
                                 {"id": "gpt-5.10-mini"}, {"id": "gpt-9-audio-preview"}]}
            return {"choices": [{"message": {"content": json.dumps(GOOD)}}]}

        result = notes.release_notes("v0.1.2", self.build, {"OPENAI_API_KEY": "k"}, transport)
        self.assertEqual(result, GOOD)
        payload = calls[1][2]
        self.assertEqual(payload["model"], "gpt-5.10")
        sent = payload["messages"][1]["content"]
        for text in ("Open menus from the board", "The palette needs no host round trip.",
                     "Fix the Linux routing", "v0.1.1 to v0.1.2"):
            self.assertIn(text, sent)
        self.assertNotIn("Old fix", sent)
        # A configured model is used as given, with no listing.
        calls.clear()
        notes.release_notes("v0.1.2", self.build,
                            {"OPENAI_API_KEY": "k", "RELEASE_NOTES_MODEL": "chosen"}, transport)
        self.assertEqual([call[2]["model"] for call in calls], ["chosen"])

    def test_a_failed_or_unusable_summary_falls_back_to_the_commits(self):
        def failing(url, key, payload=None):
            raise RuntimeError("HTTP 500 from /chat/completions")

        def answering(answer):
            def transport(url, key, payload=None):
                if url.endswith("/models"):
                    return {"data": []}
                return {"choices": [{"message": {"content": answer}}]}
            return transport

        bad_answers = [
            "not json",
            json.dumps({"title": "x"}),
            json.dumps({"title": "", "notes": GOOD["notes"]}),
            json.dumps({"title": "t" * 100, "notes": GOOD["notes"]}),
            json.dumps({"title": "ok", "notes": "short"}),
            json.dumps({"title": "ok", "notes": "n" * 5000}),
            json.dumps({"title": "ok", "notes": GOOD["notes"] + "\n\n---\nfooter"}),
        ]
        expected = notes.release_notes("v0.1.2", self.build, {})
        for transport in [failing] + [answering(answer) for answer in bad_answers]:
            self.assertEqual(
                notes.release_notes("v0.1.2", self.build, {"OPENAI_API_KEY": "k"}, transport), expected)

    def test_nothing_the_service_does_stops_the_notes(self):
        expected = notes.release_notes("v0.1.2", self.build, {})

        def raising(error):
            def transport(url, key, payload=None):
                raise error
            return transport

        def shaped(listing, reply):
            def transport(url, key, payload=None):
                return listing if url.endswith("/models") else reply
            return transport

        transports = [raising(ConnectionResetError("reset")), raising(OSError("closed")),
                      shaped([], {}), shaped({"data": ["gpt-5"]}, {}),
                      shaped({"data": []}, {"choices": []}),
                      shaped({"data": []}, {"choices": [{"message": {"content": None}}]})]
        for transport in transports:
            self.assertEqual(
                notes.release_notes("v0.1.2", self.build, {"OPENAI_API_KEY": "k"}, transport), expected)

    def test_the_command_always_writes_notes_and_exits_cleanly(self):
        output = Path(self.directory.name) / "notes.json"
        summary = Path(self.directory.name) / "summary.md"
        script = Path(__file__).with_name("release-notes.py")
        env = {key: value for key, value in os.environ.items() if key != "OPENAI_API_KEY"}

        def run(release, build):
            output.unlink(missing_ok=True)
            summary.unlink(missing_ok=True)
            done = subprocess.run(["python3", str(script), "--release", release, "--build", build,
                                   "--output", str(output), "--summary", str(summary)],
                                  env=env, capture_output=True, text=True)
            self.assertEqual(done.returncode, 0, done.stderr)
            return json.loads(output.read_text())

        self.assertEqual(run("v0.1.2", self.build)["title"], "menus and gates")
        self.assertIn("## wt v0.1.2: menus and gates", summary.read_text())
        # A preview publishes no notes and shows the draft; a test tag shows none.
        self.assertEqual(run("preview-abc", self.build), {"title": "", "notes": ""})
        self.assertIn("Draft notes for the next stable release", summary.read_text())
        self.assertEqual(run("rust-test-abc-1", self.build), {"title": "", "notes": ""})
        self.assertFalse(summary.exists())
        # A commit Git does not know still leaves a file the publisher can read.
        self.assertEqual(run("v0.1.2", "0" * 40), {"title": "", "notes": ""})

    def test_notes_cannot_notify_anyone_link_out_or_end_early(self):
        calmed = notes.assembled("v0.1.2", "v0.1.1", [
            {"subject": "Thank @someone for the report", "body": "First part.\n\n---\n\nSecond part."},
            {"subject": "Release v0.1.2: gates", "body": "- kept"}])["notes"]
        self.assertNotIn("@someone", calmed)
        self.assertIn("someone", calmed)
        self.assertNotIn("\n---\n", calmed)
        with self.assertRaisesRegex(ValueError, "link"):
            notes.checked({"title": "ok", "notes": GOOD["notes"] + " See https://example.invalid/x."})
        with self.assertRaisesRegex(ValueError, "rule"):
            notes.checked({"title": "ok", "notes": GOOD["notes"] + "\n\n***\nmore"})
        spaced = notes.checked({"title": "ok", "notes": "Menus open at once, on every row.\n**Fixed**\n- " + "A gate survives. " * 5})
        self.assertIn("row.\n\n**Fixed**", spaced["notes"])

    def test_a_long_history_keeps_its_newest_commits(self):
        sent = []

        def transport(url, key, payload=None):
            if url.endswith("/models"):
                return {"data": []}
            sent.append(payload["messages"][1]["content"])
            return {"choices": [{"message": {"content": json.dumps(GOOD)}}]}

        many = [{"subject": f"Old change {index}", "body": "x" * 2000} for index in range(60)]
        many.append({"subject": "Release v9.9.9: newest", "body": "- the summary"})
        notes.summarized("v9.9.9", "v9.9.8", many, {"OPENAI_API_KEY": "k"}, transport)
        self.assertIn("Release v9.9.9: newest", sent[0])
        self.assertNotIn("Old change 0\n", sent[0])

    def test_an_answer_is_tidied_and_a_prerelease_gets_no_notes(self):
        dash = chr(0x2014)
        tidy = notes.checked({"title": f" Faster  menus{dash}kept gates. ",
                              "notes": GOOD["notes"] + f" It waits{dash}briefly."})
        self.assertEqual(tidy["title"], "Faster menus, kept gates")
        self.assertNotIn(dash, tidy["notes"])
        for tag in ("preview-abc", "rust-test-abc-1", "v0.1.2-rc1"):
            self.assertEqual(notes.release_notes(tag, self.build, {"OPENAI_API_KEY": "k"}),
                             {"title": "", "notes": ""})

    def test_long_assembled_notes_are_cut_at_a_line(self):
        many = [{"subject": f"Change number {index} " + "x" * 60, "body": ""} for index in range(200)]
        text = notes.assembled("v9.9.9", "v9.9.8", many)["notes"]
        self.assertLessEqual(len(text), notes.MAX_NOTES + 60)
        self.assertTrue(text.endswith("- and more: see the commit history"))


if __name__ == "__main__":
    unittest.main()
