#!/usr/bin/env python3
"""Isolated smoke test for native background removal and worker guards."""

from __future__ import annotations

import json
import os
import pathlib
import pty
import re
import select
import signal
import shutil
import subprocess
import tempfile
import time


def run(args: list[str], *, env: dict[str, str], cwd: pathlib.Path | None = None) -> str:
    result = subprocess.run(args, cwd=cwd, env=env, text=True, capture_output=True)
    if result.returncode:
        raise AssertionError(
            f"command failed ({result.returncode}): {args!r}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result.stdout


def git(repo: pathlib.Path, *args: str, env: dict[str, str]) -> str:
    return run(["git", *args], cwd=repo, env=env)


def cancel_open_picker(binary: pathlib.Path, env: dict[str, str], cwd: pathlib.Path) -> float:
    pid, terminal = pty.fork()
    if pid == 0:
        os.chdir(cwd)
        os.execve(str(binary), [str(binary), "open"], env)
    transcript = bytearray()
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline and b"Open which worktree?" not in transcript:
        readable, _, _ = select.select([terminal], [], [], 0.1)
        if readable:
            try:
                transcript.extend(os.read(terminal, 4096))
            except OSError:
                break
        result = os.waitpid(pid, os.WNOHANG)
        if result[0] == pid:
            raise AssertionError(f"picker exited before prompting: {transcript.decode(errors='replace')}")
    assert b"Open which worktree?" in transcript, transcript.decode(errors="replace")
    signaled_at = time.monotonic()
    os.kill(pid, signal.SIGTERM)
    deadline = time.monotonic() + 10
    status = None
    while time.monotonic() < deadline:
        result = os.waitpid(pid, os.WNOHANG)
        if result[0] == pid:
            status = result[1]
            break
        readable, _, _ = select.select([terminal], [], [], 0.05)
        if readable:
            try:
                transcript.extend(os.read(terminal, 4096))
            except OSError:
                pass
    os.close(terminal)
    if status is None:
        os.kill(pid, signal.SIGKILL)
        _, status = os.waitpid(pid, 0)
        raise AssertionError(
            "SIGTERM left the picker blocked on terminal input: "
            + transcript.decode(errors="replace")
        )
    assert os.WIFEXITED(status) and os.WEXITSTATUS(status) != 0, (
        f"picker did not handle SIGTERM through cancellation: {status}"
    )
    return time.monotonic() - signaled_at


def main() -> int:
    binary = pathlib.Path(os.environ.get("WT_NATIVE_BIN", "target/debug/wt")).resolve()
    if not binary.is_file():
        raise SystemExit(f"native binary not found: {binary}; run `cargo build -p wt-app --locked`")
    scratch = pathlib.Path(tempfile.mkdtemp(prefix="wt-native-lifecycle-"))
    try:
        home = scratch / "home"
        config_home = home / ".config" / "wt"
        config_home.mkdir(parents=True)
        (config_home / "config.toml").write_text("\n", encoding="utf-8")
        main_repo = scratch / "main clone"
        root = scratch / "worktrees"
        lock_dir = scratch / "locks"
        main_repo.mkdir()
        root.mkdir()
        remote = scratch / "origin.git"
        env = os.environ.copy()
        env.update(
            {
                "HOME": str(home),
                "XDG_CONFIG_HOME": str(home / ".config"),
                "XDG_CACHE_HOME": str(home / ".cache"),
                "XDG_DATA_HOME": str(home / ".local" / "share"),
                "GIT_CONFIG_NOSYSTEM": "1",
                "GIT_CONFIG_GLOBAL": str(scratch / "gitconfig"),
            }
        )
        pathlib.Path(env["GIT_CONFIG_GLOBAL"]).write_text(
            "[user]\n\tname = Lifecycle Fixture\n\temail = lifecycle@example.invalid\n",
            encoding="utf-8",
        )
        run(["git", "init", "--bare", str(remote)], env=env)
        run(["git", "init", "-b", "main", str(main_repo)], env=env)
        git(main_repo, "config", "user.name", "Lifecycle Fixture", env=env)
        git(main_repo, "config", "user.email", "lifecycle@example.invalid", env=env)
        (main_repo / "tracked.txt").write_text("base\n", encoding="utf-8")
        git(main_repo, "add", "tracked.txt", env=env)
        git(main_repo, "commit", "-m", "initial", env=env)
        git(main_repo, "remote", "add", "origin", str(remote), env=env)
        git(main_repo, "push", "-u", "origin", "main", env=env)
        repo_config = main_repo / ".wt.toml"
        repo_config.write_text(
            "[paths]\n"
            f'main_clone = "{main_repo}"\n'
            f'worktree_root = "{root}"\n'
            f'lock_dir = "{lock_dir}"\n'
            f'state_db = "{scratch / "state.db"}"\n'
            "[branch]\nbase = \"main\"\nprefix = \"person\"\n",
            encoding="utf-8",
        )
        env["WT_REPO_CONFIG"] = str(repo_config)

        branch = "person/ENG-501-background"
        git(main_repo, "branch", branch, env=env)
        git(main_repo, "push", "-u", "origin", branch, env=env)
        target = root / "ENG-501-background"
        git(main_repo, "worktree", "add", str(target), branch, env=env)

        output = run([str(binary), "rm", "--background", "--yes", "ENG-501-background"], env=env, cwd=main_repo)
        match = re.search(r"\(job ([A-Za-z0-9-]+)\)", output)
        assert match, f"rm did not print an acknowledged job id: {output!r}"
        job_id = match.group(1)
        job_path = lock_dir / "destroy-jobs" / f"{job_id}.json"
        deadline = time.monotonic() + 20
        job: dict[str, object] = {}
        while time.monotonic() < deadline:
            if job_path.exists():
                job = json.loads(job_path.read_text(encoding="utf-8"))
                if job.get("state") in {"succeeded", "failed"}:
                    break
            time.sleep(0.05)
        assert job.get("state") == "succeeded", f"background job did not succeed: {job}"
        assert not target.exists(), "worker left the checkout behind"
        rows = json.loads(run([str(binary), "ls", "--json"], env=env, cwd=main_repo))
        assert any(row.get("slug") == "ENG-501-background" and row.get("kind") == "removed" for row in rows), rows
        worker_pid = int(job["workerPid"])
        alive = subprocess.run(["ps", "-p", str(worker_pid), "-o", "command="], text=True, capture_output=True)
        assert "_destroy" not in alive.stdout, f"worker leaked after success: {alive.stdout.strip()}"

        invalid = subprocess.run([str(binary), "_destroy", "../escape"], env=env, cwd=main_repo, text=True, capture_output=True)
        assert invalid.returncode != 0 and "invalid destroy job id" in invalid.stderr

        stale_branch = "person/ENG-502-stale-head"
        git(main_repo, "branch", stale_branch, env=env)
        stale_target = root / "ENG-502-stale-head"
        git(main_repo, "worktree", "add", str(stale_target), stale_branch, env=env)
        picker_branch = "person/ENG-503-picker"
        git(main_repo, "branch", picker_branch, env=env)
        git(main_repo, "worktree", "add", str(root / "ENG-503-picker"), picker_branch, env=env)
        picker_exit_seconds = cancel_open_picker(binary, env, main_repo)
        assert picker_exit_seconds < 2.0, f"picker cancellation took too long: {picker_exit_seconds:.3f}s"
        inventory = json.loads(run([str(binary), "_inventory"], env=env, cwd=main_repo))
        record = next(item["worktree"] for item in inventory if item["worktree"]["target"]["slug"] == "ENG-502-stale-head")
        stale_job_id = "stale-head-check"
        stale_job = {
            "version": 1,
            "id": stale_job_id,
            "target": {
                "key": record["target"]["slug"],
                "path": record["target"]["path"],
                "branch": record["target"]["branch"],
                "head": "0" * 40,
                "revision": {
                    **job["target"]["revision"],
                    "key": record["target"]["slug"],
                    "path": record["target"]["path"],
                    "branch": record["target"]["branch"],
                    "head": "0" * 40,
                },
            },
            "operation": "remove",
            "options": {"force": True, "deleteBranch": True, "landed": False, "destroyStage": False},
            "createdAt": "fixture",
            "workerPid": None,
            "workerStartIdentity": None,
            "state": "starting",
            "error": None,
            "logPath": str(lock_dir / "destroy-jobs" / f"{stale_job_id}.log"),
            "completedAt": None,
        }
        stale_dir = lock_dir / "destroy-jobs"
        stale_dir.mkdir(parents=True, exist_ok=True)
        (stale_dir / f"{stale_job_id}.json").write_text(json.dumps(stale_job), encoding="utf-8")
        refused = subprocess.run([str(binary), "_destroy", stale_job_id], env=env, cwd=main_repo, text=True, capture_output=True)
        assert refused.returncode != 0 and "HEAD changed" in refused.stderr, refused.stderr
        assert stale_target.exists(), "worker removed a checkout whose HEAD did not match the job"
        print(
            "native lifecycle smoke passed: ack, durable removal, no worker leak, path/head guards, "
            f"PTY picker SIGTERM exit in {picker_exit_seconds * 1000:.1f} ms"
        )
        return 0
    finally:
        shutil.rmtree(scratch, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
