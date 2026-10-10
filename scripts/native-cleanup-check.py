#!/usr/bin/env python3
"""Exercise native cleanup against real Git landing and retention hazards."""

from __future__ import annotations

import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile


def main() -> None:
    binary = Path(os.environ.get("WT_NATIVE_BIN", "target/debug/wt")).resolve()
    if not binary.is_file():
        raise SystemExit("build wt-app before running the cleanup check")
    scratch = Path(tempfile.mkdtemp(prefix="wt-native-cleanup-"))
    try:
        home = scratch / "home"
        repo = scratch / "main"
        worktrees = scratch / "worktrees"
        for path in [home / ".config/wt", repo, worktrees]:
            path.mkdir(parents=True)
        (home / ".config/wt/config.toml").write_text("")
        env = {key: value for key, value in os.environ.items() if not key.startswith("WT_")}
        env.update({
            "HOME": str(home), "XDG_CONFIG_HOME": str(home / ".config"),
            "XDG_DATA_HOME": str(home / ".local/share"), "XDG_CACHE_HOME": str(home / ".cache"),
            "GIT_CONFIG_NOSYSTEM": "1", "GIT_CONFIG_GLOBAL": str(scratch / "gitconfig"),
            "WT_GITHUB": "off", "WT_AUTO_UPDATE": "off",
        })
        Path(env["GIT_CONFIG_GLOBAL"]).write_text(
            "[user]\nname = Cleanup Fixture\nemail = cleanup@example.invalid\n"
            "[core]\nhooksPath = /dev/null\n"
        )

        def run(*args: str, cwd: Path = repo, expected: int = 0) -> str:
            result = subprocess.run(args, cwd=cwd, env=env, capture_output=True, text=True, timeout=45)
            assert result.returncode == expected, (args, result.returncode, result.stdout, result.stderr)
            return result.stdout

        def git(*args: str, cwd: Path = repo) -> str:
            return run("git", *args, cwd=cwd)

        remote = scratch / "origin.git"
        git("init", "--bare", str(remote))
        git("init", "-b", "main")
        (repo / "initial.txt").write_text("initial\n")
        git("add", "initial.txt")
        git("commit", "-m", "initial")
        git("remote", "add", "origin", str(remote))
        git("push", "-u", "origin", "main")
        config = repo / ".wt.toml"
        config.write_text(
            f'[paths]\nmain_clone = "{repo}"\nworktree_root = "{worktrees}"\n'
            f'lock_dir = "{scratch / "locks"}"\nstate_db = "{scratch / "state.sqlite"}"\n'
            '[branch]\nbase = "main"\nprefix = "fixture"\n'
        )
        env["WT_REPO_CONFIG"] = str(config)
        for slug in ["landed", "dirty", "verification", "empty", "unlanded"]:
            path = worktrees / slug
            git("worktree", "add", "-b", f"fixture/{slug}", str(path), "main")
            run(str(binary), "base", "clear", slug)
            if slug != "empty":
                (path / f"{slug}.txt").write_text(f"{slug} work\n")
                git("add", ".", cwd=path)
                git("commit", "-m", f"{slug} work", cwd=path)
            git("push", "-u", "origin", f"fixture/{slug}", cwd=path)
        for slug in ["landed", "dirty", "verification"]:
            git("merge", "--no-edit", f"fixture/{slug}")
        git("push", "origin", "main")
        (worktrees / "dirty/untracked.txt").write_text("keep this file\n")
        run(str(binary), "status", "verification", "ready", "--risk", "low",
            "--verify-after-merge", "Confirm fixture environment after deployment")
        output = run(str(binary), "clean", "--yes", "--foreground", "--no-destroy-stage")
        assert "removed landed" in output, output
        assert not (worktrees / "landed").exists(), output
        for slug in ["dirty", "verification", "empty", "unlanded"]:
            assert (worktrees / slug).is_dir(), (slug, output)
        assert (worktrees / "dirty/untracked.txt").read_text() == "keep this file\n"
        rows = json.loads(run(str(binary), "ls", "--json"))
        assert any(row["slug"] == "landed" and row["kind"] == "removed" for row in rows), rows
        assert "Confirm fixture environment" in run(str(binary), "status", "verification")
        output = run(str(binary), "clean", "--yes", "--foreground", "--no-destroy-stage")
        assert "Nothing to clean" in output, output
        print(json.dumps({"merged_removed": True, "dirty_preserved": True,
                          "verification_preserved": True, "empty_branch_preserved": True,
                          "unlanded_preserved": True, "history_retained": True,
                          "repeat_idempotent": True}))
    finally:
        shutil.rmtree(scratch)


if __name__ == "__main__":
    main()
