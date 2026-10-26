#!/usr/bin/env python3
"""Unit tests for restore-mtimes.py against a throwaway Git repository."""

import importlib.util
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

spec = importlib.util.spec_from_file_location("restore_mtimes", Path(__file__).with_name("restore-mtimes.py"))
restore = importlib.util.module_from_spec(spec)
spec.loader.exec_module(restore)


def git(repo, *args, when=None):
    env = {**os.environ, "GIT_CONFIG_GLOBAL": os.devnull, "GIT_CONFIG_NOSYSTEM": "1"}
    if when is not None:
        env.update(GIT_AUTHOR_DATE=f"@{when} +0000", GIT_COMMITTER_DATE=f"@{when} +0000")
    subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@example.invalid", *args],
                   cwd=repo, env=env, check=True, capture_output=True)


class RestoreMtimesTests(unittest.TestCase):
    def test_newest_commit_wins_per_path(self):
        log = "\x00300\n\nb.rs\n\x00200\n\na.rs\nb.rs\n"
        self.assertEqual(restore.commit_times(log), {"b.rs": 300, "a.rs": 200})

    def test_files_take_their_last_commit_time(self):
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            git(repo, "init", "-q", "-b", "main")
            (repo / "old.rs").write_text("old\n")
            (repo / "space name.rs").write_text("x\n")
            git(repo, "add", ".")
            git(repo, "commit", "-q", "-m", "one", when=1_000_000_000)
            (repo / "new.rs").write_text("new\n")
            git(repo, "add", ".")
            git(repo, "commit", "-q", "-m", "two", when=1_000_000_500)
            cwd = os.getcwd()
            os.chdir(repo)
            try:
                self.assertEqual(restore.main(), 0)
            finally:
                os.chdir(cwd)
            self.assertEqual(int((repo / "old.rs").stat().st_mtime), 1_000_000_000)
            self.assertEqual(int((repo / "space name.rs").stat().st_mtime), 1_000_000_000)
            self.assertEqual(int((repo / "new.rs").stat().st_mtime), 1_000_000_500)

    def test_shallow_clone_dates_every_file_at_head(self):
        # Correct but useless for caching: nothing looks older than the cache,
        # so CI checks out full history (fetch-depth: 0).
        with tempfile.TemporaryDirectory() as directory:
            origin, clone = Path(directory) / "origin", Path(directory) / "clone"
            origin.mkdir()
            git(origin, "init", "-q", "-b", "main")
            (origin / "a.rs").write_text("a\n")
            git(origin, "add", ".")
            git(origin, "commit", "-q", "-m", "one", when=1_000_000_000)
            (origin / "b.rs").write_text("b\n")
            git(origin, "add", ".")
            git(origin, "commit", "-q", "-m", "two", when=1_000_000_500)
            subprocess.run(["git", "clone", "-q", "--depth", "1", f"file://{origin}", str(clone)],
                           check=True, capture_output=True)
            cwd = os.getcwd()
            os.chdir(clone)
            try:
                self.assertEqual(restore.main(), 0)
            finally:
                os.chdir(cwd)
            self.assertEqual(int((clone / "a.rs").stat().st_mtime), 1_000_000_500)


if __name__ == "__main__":
    unittest.main()
