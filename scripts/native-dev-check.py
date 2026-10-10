#!/usr/bin/env python3
"""Exercise native wt dev-server lifecycle in a private git/tmux fixture."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import signal
import shlex
import shutil
import subprocess
import sys
import tempfile
import time


ROOT = Path(__file__).resolve().parents[1]


def run(argv: list[str], *, cwd: Path, env: dict[str, str], expected: int = 0) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(argv, cwd=cwd, env=env, text=True, capture_output=True, timeout=45)
    if result.returncode != expected:
        raise AssertionError(
            f"{argv!r} exited {result.returncode}, expected {expected}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result


def git(argv: list[str], *, cwd: Path, env: dict[str, str]) -> None:
    subprocess.run(argv, cwd=cwd, env=env, check=True, capture_output=True, timeout=20)


def toml_string(value: str) -> str:
    return json.dumps(value)


def assert_process_reaped(pid_file: Path) -> None:
    assert pid_file.is_file(), f"dev process did not publish its pid: {pid_file}"
    pid = int(pid_file.read_text(encoding="utf-8"))
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return
    raise AssertionError(f"dev child process {pid} survived its synchronous stop")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/wt")
    args = parser.parse_args()
    binary = args.binary.resolve()
    if not binary.is_file():
        raise SystemExit(f"native wt binary not found: {binary}; build with `cargo build -p wt-app --locked`")

    with tempfile.TemporaryDirectory(prefix="wt-native-dev-check-") as temporary:
        scratch = Path(temporary)
        home = scratch / "home"
        home.mkdir()
        # tmux concatenates TMPDIR, uid and socket name into a unix-domain
        # socket path, so keep this isolated directory deliberately short.
        tmux_tmp = Path(tempfile.mkdtemp(prefix="wtd-", dir="/tmp"))
        main_clone = scratch / "main clone"
        worktree_root = scratch / "worktrees"
        worktree_root.mkdir()
        git_env = dict(os.environ, HOME=str(home), GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=str(scratch / "gitconfig"))
        Path(git_env["GIT_CONFIG_GLOBAL"]).write_text(
            '[user]\n\tname = "Native dev fixture"\n\temail = dev@example.invalid\n', encoding="utf-8"
        )
        git(["git", "init", "-b", "main", str(main_clone)], cwd=scratch, env=git_env)
        (main_clone / "tracked.txt").write_text("fixture\n", encoding="utf-8")
        git(["git", "-C", str(main_clone), "add", "tracked.txt"], cwd=scratch, env=git_env)
        git(["git", "-C", str(main_clone), "commit", "-m", "fixture"], cwd=scratch, env=git_env)
        worktrees = ["dev-one", "dev-two"]
        for slug in worktrees:
            git(["git", "-C", str(main_clone), "worktree", "add", "-b", f"fixture/{slug}", str(worktree_root / slug), "main"], cwd=scratch, env=git_env)

        repo_config = scratch / "wt.toml"
        state_db = scratch / "state.sqlite"
        cache_root = scratch / "cache"
        lock_dir = scratch / "locks"
        socket = f"wt-dev-check-{os.getpid()}"
        python = shlex.quote(sys.executable)
        probe = (
            "import urllib.request; "
            "opener=urllib.request.build_opener(urllib.request.ProxyHandler({})); "
            "opener.open('http://127.0.0.1:{{port}}/', timeout=2).read()"
        )
        healthy = f"{python} -c {shlex.quote(probe)}"
        pid_file = scratch / "dev-child.pid"
        server_file = scratch / "server.py"
        server_file.write_text(
            "import http.server, os, pathlib, sys\n"
            "port = int(sys.argv[1])\n"
            f"pathlib.Path({str(pid_file)!r}).write_text(str(os.getpid()))\n"
            "class Handler(http.server.BaseHTTPRequestHandler):\n"
            "    def do_GET(self):\n"
            "        self.send_response(200)\n"
            "        self.end_headers()\n"
            "        self.wfile.write(b'ok\\n')\n"
            "    def log_message(self, *args): pass\n"
            "http.server.HTTPServer(('127.0.0.1', port), Handler).serve_forever()\n",
            encoding="utf-8",
        )
        settings = {
            "command": f"{python} {toml_string(str(server_file))} {{{{port}}}}",
            "port_base": 38100,
            "port_range": 64,
            "url": "http://127.0.0.1:{{port}}/",
            "max_concurrent": 1,
            "health_command": healthy,
        }

        def write_config() -> None:
            lines = [
                "[paths]",
                f"main_clone = {toml_string(str(main_clone))}",
                f"worktree_root = {toml_string(str(worktree_root))}",
                f"state_db = {toml_string(str(state_db))}",
                f"cache_db = {toml_string(str(cache_root / 'cache.sqlite'))}",
                f"lock_dir = {toml_string(str(lock_dir))}",
                f"log_dir = {toml_string(str(scratch / 'logs'))}",
                f"app_log_dir = {toml_string(str(scratch / 'app-logs'))}",
                f"dotfiles = {toml_string(str(scratch / 'dotfiles'))}",
                "[branch]",
                'base = "main"',
                'prefix = "fixture/"',
                f"[tmux]\nsocket = {toml_string(socket)}",
                "[dev_server]",
            ]
            lines.extend(f"{key} = {toml_string(value) if isinstance(value, str) else value}" for key, value in settings.items())
            repo_config.write_text("\n".join(lines) + "\n", encoding="utf-8")

        write_config()
        env = dict(os.environ)
        for key in list(env):
            if key.startswith("WT_") or key in {"TMUX", "TMUX_PANE", "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_DATA_HOME", "GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"}:
                env.pop(key, None)
        env.update(
            {
                "HOME": str(home),
                # Use the CI shell explicitly; the host user's login shell
                # otherwise leaks into nested `SHELL -lc` dev and health runs.
                "SHELL": "/bin/bash",
                "XDG_CONFIG_HOME": str(home / ".config"),
                "XDG_CACHE_HOME": str(cache_root),
                "XDG_DATA_HOME": str(home / ".local" / "share"),
                "TMUX_TMPDIR": str(tmux_tmp),
                "GIT_CONFIG_NOSYSTEM": "1",
                "GIT_CONFIG_GLOBAL": str(scratch / "gitconfig"),
                "WT_REPO_CONFIG": str(repo_config),
                "WT_UPDATE": "off",
                "WT_SKILLS": "off",
                "WT_GITHUB": "off",
            }
        )

        def wt(*argv: str, expected: int = 0) -> subprocess.CompletedProcess[str]:
            try:
                return run([str(binary), *argv], cwd=main_clone, env=env, expected=expected)
            except (AssertionError, subprocess.TimeoutExpired):
                # Capture evidence while the isolated server still exists.
                # The finally block deliberately tears it down on failure.
                status_json: dict[str, object] | None = None
                for diagnostic in (
                    [str(binary), "dev", "status", "--all", "--json"],
                    [str(binary), "dev", "logs", "dev-one"],
                    ["tmux", "-L", socket, "capture-pane", "-p", "-t", "=dev-one-dev:"],
                ):
                    try:
                        result = subprocess.run(diagnostic, cwd=main_clone, env=env, text=True, capture_output=True, timeout=10)
                        print(f"fixture diagnostic {diagnostic!r}:\n{result.stdout}\n{result.stderr}", flush=True)
                        if diagnostic[1:4] == ["dev", "status", "--all"] and result.returncode == 0:
                            try:
                                status_json = json.loads(result.stdout)
                            except json.JSONDecodeError:
                                pass
                    except subprocess.TimeoutExpired:
                        print(f"fixture diagnostic timed out: {diagnostic!r}", flush=True)
                # `wt dev status` intentionally keeps only the first nonempty
                # health-output line. On failure that is often just Python's
                # generic Traceback header, so execute the exact check once
                # more and retain its complete, bounded diagnostic here.
                if status_json is not None:
                    rows = status_json.get("worktrees", [])
                    row = next(
                        (item for item in rows if isinstance(item, dict) and item.get("slug") == "dev-one"),
                        None,
                    ) if isinstance(rows, list) else None
                    status = row.get("status") if isinstance(row, dict) else None
                    port = status.get("port") if isinstance(status, dict) else None
                    health_template = settings.get("health_command")
                    if isinstance(port, int) and isinstance(health_template, str):
                        health = health_template.replace("{{port}}", str(port))
                        shell = env.get("SHELL", "/bin/bash")
                        try:
                            result = subprocess.run(
                                [shell, "-lc", health],
                                cwd=worktree_root / "dev-one",
                                env=dict(env, PORT=str(port)),
                                text=True,
                                capture_output=True,
                                timeout=5,
                            )
                            print(
                                f"fixture exact health diagnostic (exit {result.returncode}):\n"
                                f"stdout:\n{result.stdout[-4000:]}\nstderr:\n{result.stderr[-4000:]}",
                                flush=True,
                            )
                        except subprocess.TimeoutExpired as error:
                            print(f"fixture exact health diagnostic timed out: {error}", flush=True)
                if pid_file.is_file():
                    try:
                        pid = pid_file.read_text(encoding="utf-8").strip()
                        result = subprocess.run(
                            ["ps", "-p", pid, "-o", "pid=,ppid=,stat=,command="],
                            text=True,
                            capture_output=True,
                            timeout=5,
                        )
                        print(f"fixture dev-child process diagnostic:\n{result.stdout}\n{result.stderr}", flush=True)
                    except (OSError, subprocess.TimeoutExpired) as error:
                        print(f"fixture dev-child process diagnostic failed: {error}", flush=True)
                raise

        waiter: subprocess.Popen[str] | None = None
        try:
            # A worker must supply its command to tmux, without typing into an
            # interactive shell whose startup may swallow input. A custom
            # default command makes that regression deterministic.
            run(["tmux", "-L", socket, "new-session", "-d", "-s", "fixture-bootstrap", "cat"], cwd=main_clone, env=env)
            run(["tmux", "-L", socket, "set-option", "-g", "default-command", "cat"], cwd=main_clone, env=env)
            first = wt("dev", "start", "dev-one", "--wait", "--timeout", "20")
            status = json.loads(wt("dev", "status", "dev-one", "--json").stdout)
            assert status["status"]["running"] is True, status
            if not status["health"]["ok"]:
                port = status["status"]["port"]
                command = settings["health_command"].replace("{{port}}", str(port))
                diagnostic = subprocess.run(
                    [env["SHELL"], "-lc", command],
                    cwd=worktree_root / "dev-one",
                    env=dict(env, PORT=str(port)),
                    text=True,
                    capture_output=True,
                    timeout=5,
                )
                print(
                    f"fixture exact health diagnostic (exit {diagnostic.returncode}):\n"
                    f"stdout:\n{diagnostic.stdout[-4000:]}\nstderr:\n{diagnostic.stderr[-4000:]}",
                    flush=True,
                )
                if pid_file.is_file():
                    pid = pid_file.read_text(encoding="utf-8").strip()
                    process = subprocess.run(
                        ["ps", "-p", pid, "-o", "pid=,ppid=,stat=,command="],
                        text=True,
                        capture_output=True,
                        timeout=5,
                    )
                    print(f"fixture dev-child process diagnostic:\n{process.stdout}\n{process.stderr}", flush=True)
            assert status["health"]["ok"] is True, status
            port = status["status"]["port"]
            assert port and str(port) in first.stdout, first.stdout

            # A held slot times out with the stable temporary-refusal code.
            full = wt("dev", "start", "dev-two", "--wait", "--timeout", "1", expected=75)
            assert "no dev-server slot" in full.stderr, full.stderr
            queue_dir = cache_root / "dev" / "waiting"
            assert not (queue_dir / "dev-two.json").exists(), "timed-out waiter remained queued"

            # SIGTERM cancels the queue wait promptly and removes its waiter.
            waiter = subprocess.Popen(
                [str(binary), "dev", "start", "dev-two", "--wait", "--timeout", "30"],
                cwd=main_clone,
                env=env,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            waiter_file = queue_dir / "dev-two.json"
            deadline = time.monotonic() + 8
            while time.monotonic() < deadline and not waiter_file.exists():
                if waiter.poll() is not None:
                    break
                time.sleep(0.02)
            if not waiter_file.exists():
                if waiter.poll() is None:
                    waiter.kill()
                output, error = waiter.communicate(timeout=5)
                raise AssertionError(
                    "wait command never entered the queue; "
                    f"exit={waiter.returncode} stdout={output!r} stderr={error!r}"
                )
            signaled_at = time.monotonic()
            waiter.send_signal(signal.SIGTERM)
            waiter.communicate(timeout=5)
            elapsed = time.monotonic() - signaled_at
            assert elapsed < 2, f"SIGTERM queue cancellation took {elapsed:.3f}s"
            assert not waiter_file.exists(), "cancelled waiter left stale queue state"

            wt("dev", "stop", "dev-one")
            assert_process_reaped(pid_file)
            assert not subprocess.run(["tmux", "-L", socket, "has-session", "-t", "=dev-one-dev"], env=env, capture_output=True).returncode == 0

            # The waiter can claim the released slot and become ready.
            wt("dev", "start", "dev-two", "--wait", "--timeout", "20")
            settings["stop_command"] = "exit 17"
            settings["reset_command"] = f"touch {toml_string(str(scratch / 'reset-ran'))}"
            write_config()
            reset = wt("dev", "reset", "dev-two", expected=1)
            assert_process_reaped(pid_file)
            assert "reset_command was not run" in reset.stderr, reset.stderr
            assert not (scratch / "reset-ran").exists(), "reset hook ran after stop hook failure"

            # Removal must stop the supervised server under the lifecycle
            # lock, and a failed teardown must leave the checkout intact.
            wt("dev", "start", "dev-two")
            failed_remove = wt("rm", "--yes", "--force", "dev-two", expected=1)
            assert_process_reaped(pid_file)
            assert (worktree_root / "dev-two").is_dir(), "failed teardown removed the checkout"
            assert "stop_command failed" in failed_remove.stderr, failed_remove.stderr
            settings.pop("stop_command")
            write_config()
            wt("dev", "start", "dev-one")
            wt("rm", "--yes", "--force", "dev-one")
            assert_process_reaped(pid_file)
            assert not (worktree_root / "dev-one").exists(), "successful removal left its checkout"
            assert subprocess.run(
                ["tmux", "-L", socket, "has-session", "-t", "=dev-one-dev"],
                env=env,
                capture_output=True,
            ).returncode != 0, "removed worktree left its dev session running"

            # A quickly repeating failed command is parked and reported as a crash.
            settings["command"] = "echo fixture-failure; exit 1"
            settings.pop("stop_command", None)
            settings.pop("reset_command", None)
            settings.pop("health_command", None)
            write_config()
            failed = wt("dev", "start", "dev-two", "--wait", "--timeout", "30", expected=1)
            assert "crashed" in failed.stderr or "not ready" in failed.stderr, failed.stderr
            status = json.loads(wt("dev", "status", "dev-two", "--json").stdout)
            assert status["status"]["crashed"] is True, status
            logs = wt("dev", "logs", "dev-two").stdout
            assert "fixture-failure" in logs or "exited" in logs, logs
            print(f"native dev checks passed; SIGTERM queue cancellation {elapsed * 1000:.1f}ms; allocated port {port}")
        finally:
            if waiter is not None and waiter.poll() is None:
                waiter.kill()
                waiter.communicate(timeout=5)
            subprocess.run(["tmux", "-L", socket, "kill-server"], env=env, capture_output=True)
            shutil.rmtree(tmux_tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
