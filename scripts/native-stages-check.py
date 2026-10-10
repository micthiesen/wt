#!/usr/bin/env python3
"""Exercise `wt stages` against private Git fixtures and fake AWS/pnpm tools."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile


ROOT = Path(__file__).resolve().parents[1]


def run(argv: list[str], *, cwd: Path, env: dict[str, str], expected: int = 0) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(argv, cwd=cwd, env=env, text=True, capture_output=True, timeout=60)
    if result.returncode != expected:
        raise AssertionError(
            f"{argv!r} exited {result.returncode}, expected {expected}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result


def git(*args: str, cwd: Path, env: dict[str, str]) -> None:
    subprocess.run(["git", *args], cwd=cwd, env=env, check=True, capture_output=True, timeout=20)


def toml(value: str) -> str:
    return json.dumps(value)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/wt")
    args = parser.parse_args()
    binary = args.binary.resolve()
    if not binary.is_file():
        raise SystemExit(f"native wt binary not found: {binary}; build with `cargo build -p wt-app --locked`")

    with tempfile.TemporaryDirectory(prefix="wt-native-stages-") as temporary:
        scratch = Path(temporary)
        home = scratch / "home"
        home.mkdir()
        main_clone = scratch / "main clone"
        worktree_root = scratch / "worktrees"
        worktree_root.mkdir()
        bin_dir = scratch / "bin"
        bin_dir.mkdir()
        logs = scratch / "pnpm-args.jsonl"
        aws = bin_dir / "aws"
        aws.write_text(
            "#!/usr/bin/env python3\n"
            "import json, pathlib, sys\n"
            "args = sys.argv[1:]\n"
            "if args[-2:] != ['--profile', 'fixture']:\n"
            " print('unexpected profile argv: ' + repr(args), file=sys.stderr); sys.exit(92)\n"
            "if args[:2] == ['s3', 'ls']:\n"
            " print('2026-10-08 10:00:00 12 m-live.json')\n"
            " print('2026-10-08 09:00:00 20 m-orphan-a.json')\n"
            " print('2026-10-08 08:00:00 20 m-orphan-b.json')\n"
            " print('2026-10-08 07:00:00  3 m-empty.json')\n"
            " print('2026-10-08 06:00:00 20 m-unknown.json')\n"
            " print('2026-10-08 05:00:00 20 m-personal.json')\n"
            " print('2026-10-08 04:00:00 20 foreign-stage.json')\n"
            "elif args[:2] == ['s3', 'cp']:\n"
            " stage = pathlib.PurePosixPath(args[2]).name\n"
            " if stage == 'm-empty.json': print('{\"checkpoint\":{\"latest\":{\"resources\":[]}}}')\n"
            " elif stage == 'm-unknown.json': print('{broken')\n"
            " else: print('{\"checkpoint\":{\"latest\":{\"resources\":[{\"urn\":\"fixture\"}]}}}')\n"
            "else:\n"
            " print('unexpected aws argv: ' + repr(args), file=sys.stderr); sys.exit(91)\n",
            encoding="utf-8",
        )
        pnpm = bin_dir / "pnpm"
        pnpm.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, pathlib, subprocess, sys\n"
            "args = sys.argv[1:]\n"
            "with open(os.environ['WT_STAGES_PNPM_LOG'], 'a', encoding='utf-8') as f: f.write(json.dumps(args) + '\\n')\n"
            "stage = args[args.index('--stage') + 1]\n"
            "if stage == 'm-orphan-a' and os.environ.get('WT_STAGES_RACE') == '1':\n"
            " main = pathlib.Path(os.environ['WT_STAGES_MAIN'])\n"
            " target = pathlib.Path(os.environ['WT_STAGES_WORKTREES']) / 'racing-live'\n"
            " subprocess.run(['git', '-C', str(main), 'worktree', 'add', '-b', 'fixture/racing-live', str(target), 'HEAD'], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)\n"
            " (target / '.sst').mkdir(exist_ok=True)\n"
            " (target / '.sst/stage').write_text('m-orphan-b\\n', encoding='utf-8')\n"
            "",
            encoding="utf-8",
        )
        aws.chmod(0o755)
        pnpm.chmod(0o755)

        env = dict(os.environ)
        for key in list(env):
            if key.startswith("WT_") or key in {"TMUX", "TMUX_PANE", "GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"}:
                env.pop(key, None)
        env.update(
            {
                "HOME": str(home),
                "XDG_CONFIG_HOME": str(home / ".config"),
                "XDG_CACHE_HOME": str(home / ".cache"),
                "XDG_DATA_HOME": str(home / ".local/share"),
                "PATH": str(bin_dir) + os.pathsep + env["PATH"],
                "GIT_CONFIG_NOSYSTEM": "1",
                "GIT_CONFIG_GLOBAL": str(scratch / "gitconfig"),
                "WT_STAGES_PNPM_LOG": str(logs),
                "WT_STAGES_MAIN": str(main_clone),
                "WT_STAGES_WORKTREES": str(worktree_root),
            }
        )
        Path(env["GIT_CONFIG_GLOBAL"]).write_text(
            '[user]\n\tname = "Native stages fixture"\n\temail = stages@example.invalid\n', encoding="utf-8"
        )
        git("init", "-b", "main", str(main_clone), cwd=scratch, env=env)
        (main_clone / "fixture.txt").write_text("fixture\n", encoding="utf-8")
        git("-C", str(main_clone), "add", "fixture.txt", cwd=scratch, env=env)
        git("-C", str(main_clone), "commit", "-m", "fixture", cwd=scratch, env=env)
        live = worktree_root / "live"
        git("-C", str(main_clone), "worktree", "add", "-b", "fixture/live", str(live), "HEAD", cwd=scratch, env=env)
        (live / ".sst").mkdir()
        (live / ".sst/stage").write_text("m-live\n", encoding="utf-8")

        repo_config = scratch / "wt.toml"
        repo_config.write_text(
            "[paths]\n"
            f"main_clone = {toml(str(main_clone))}\n"
            f"worktree_root = {toml(str(worktree_root))}\n"
            f"state_db = {toml(str(scratch / 'state.sqlite'))}\n"
            f"cache_db = {toml(str(scratch / 'cache.sqlite'))}\n"
            f"lock_dir = {toml(str(scratch / 'locks'))}\n"
            f"log_dir = {toml(str(scratch / 'logs'))}\n"
            f"app_log_dir = {toml(str(scratch / 'app-logs'))}\n"
            "[branch]\nbase = \"main\"\nprefix = \"fixture/\"\n"
            "[stage]\nprefix = \"m-\"\ndefault_personal = \"m-personal\"\ndomain = \"preview.example.invalid\"\n"
            "[deploy.sst]\nstate_bucket = \"fixture-state\"\nstate_prefix = \"apps/\"\naws_profile = \"fixture\"\n",
            encoding="utf-8",
        )
        env["WT_REPO_CONFIG"] = str(repo_config)

        listed = run([str(binary), "stages", "--json"], cwd=main_clone, env=env)
        inventory = json.loads(listed.stdout)
        assert [row["name"] for row in inventory["live"]] == ["m-live"], inventory
        assert [row["name"] for row in inventory["orphaned"]] == ["m-orphan-a", "m-orphan-b"], inventory
        assert [row["name"] for row in inventory["unknown"]] == ["m-unknown"], inventory
        all_names = {row["name"] for group in (inventory["live"], inventory["orphaned"]) for row in group}
        assert "m-empty" not in all_names and "m-personal" not in all_names and "foreign-stage" not in all_names, inventory

        confirmation = run([str(binary), "stages", "--clean"], cwd=main_clone, env=env, expected=2)
        assert "Use -y" in confirmation.stderr, confirmation.stderr

        env["WT_STAGES_RACE"] = "1"
        cleaned = run([str(binary), "stages", "--clean", "--yes", "--json"], cwd=main_clone, env=env, expected=1)
        assert json.loads(cleaned.stdout)["orphaned"], cleaned.stdout
        assert "Skipping m-orphan-b" in cleaned.stderr, cleaned.stderr
        calls = [json.loads(line) for line in logs.read_text(encoding="utf-8").splitlines()]
        assert calls == [["sst", "remove", "--stage", "m-orphan-a"]], calls
        after_race = json.loads(run([str(binary), "stages", "--json"], cwd=main_clone, env=env).stdout)
        assert "m-orphan-b" in [row["name"] for row in after_race["live"]], after_race

    print("native wt stages fixture passed")


if __name__ == "__main__":
    main()
