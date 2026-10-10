#!/usr/bin/env python3
"""Exercise worktree-owned cleanup in an isolated git/tmux fixture."""

from __future__ import annotations

import json
import os
from pathlib import Path
import shlex
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import time


ROOT = Path(__file__).resolve().parents[1]


def run(
    argv: list[str],
    *,
    cwd: Path,
    env: dict[str, str],
    expected: int = 0,
    timeout: float = 30,
) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(
        argv, cwd=cwd, env=env, text=True, capture_output=True, timeout=timeout
    )
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


def tmux(env: dict[str, str], socket_name: str, *args: str) -> subprocess.CompletedProcess[str]:
    return run(["tmux", "-L", socket_name, *args], cwd=Path(env["HOME"]), env=env)


def assert_session(env: dict[str, str], socket_name: str, name: str, exists: bool) -> None:
    result = subprocess.run(
        ["tmux", "-L", socket_name, "has-session", "-t", f"={name}"],
        cwd=Path(env["HOME"]),
        env=env,
        text=True,
        capture_output=True,
        timeout=5,
    )
    assert (result.returncode == 0) is exists, (
        f"tmux session {name!r} expected exists={exists}; "
        f"stdout={result.stdout!r} stderr={result.stderr!r}"
    )


def start_listener(server: Path, cwd: Path, record: Path, env: dict[str, str]) -> subprocess.Popen[str]:
    process = subprocess.Popen(
        [sys.executable, str(server), "0", str(record)],
        cwd=cwd,
        env=env,
        text=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if record.is_file():
            return process
        if process.poll() is not None:
            stderr = process.stderr.read() if process.stderr else ""
            raise AssertionError(f"fixture listener exited early: {stderr}")
        time.sleep(0.02)
    raise AssertionError(f"fixture listener did not publish ownership record: {record}")


def wait_exit(process: subprocess.Popen[str], label: str, timeout: float = 6) -> None:
    try:
        process.wait(timeout=timeout)
    except subprocess.TimeoutExpired as error:
        raise AssertionError(f"{label} survived cleanup (pid {process.pid})") from error


def listener_pids(port: int) -> list[int]:
    result = subprocess.run(
        ["lsof", "-nP", f"-iTCP:{port}", "-sTCP:LISTEN", "-t"],
        text=True,
        capture_output=True,
        timeout=5,
    )
    return [int(line) for line in result.stdout.splitlines() if line.isdigit()]


def assert_pids_reaped(pids: list[int], ps: str) -> None:
    deadline = time.monotonic() + 6
    remaining = set(pids)
    while remaining and time.monotonic() < deadline:
        for pid in list(remaining):
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                remaining.remove(pid)
                continue
            state = subprocess.run(
                [ps, "-p", str(pid), "-o", "stat="],
                text=True,
                capture_output=True,
                timeout=3,
            ).stdout.strip()
            if not state or state.startswith("Z"):
                remaining.remove(pid)
        if remaining:
            time.sleep(0.05)
    assert not remaining, f"listener processes survived removal: {sorted(remaining)}"


def main() -> None:
    binary = (Path(sys.argv[1]) if len(sys.argv) > 1 else ROOT / "target/debug/wt").resolve()
    if not binary.is_file():
        raise SystemExit(f"native wt binary not found: {binary}; build the current wt-app first")
    for dependency in ("git", "tmux", "lsof"):
        if not shutil.which(dependency):
            raise SystemExit(f"native cleanup fixture requires {dependency} on PATH")

    with tempfile.TemporaryDirectory(prefix="wt-cleanup-check-", dir="/tmp") as temporary:
        scratch = Path(temporary)
        home = scratch / "home"
        home.mkdir()
        tmux_tmp = Path(tempfile.mkdtemp(prefix="wtc-", dir="/tmp"))
        main_clone = scratch / "main clone"
        worktree_root = scratch / "worktrees"
        worktree_root.mkdir()
        config = scratch / "wt.toml"
        state_db = scratch / "state.sqlite"
        cache_root = scratch / "cache"
        lock_dir = scratch / "locks"
        log_dir = scratch / "logs"
        socket_name = f"wt-clean-{os.getpid()}"
        git_env = dict(
            os.environ,
            HOME=str(home),
            GIT_CONFIG_NOSYSTEM="1",
            GIT_CONFIG_GLOBAL=str(scratch / "gitconfig"),
        )
        Path(git_env["GIT_CONFIG_GLOBAL"]).write_text(
            '[user]\n\tname = "Native cleanup fixture"\n\temail = cleanup@example.invalid\n',
            encoding="utf-8",
        )
        git(["git", "init", "-b", "main", str(main_clone)], cwd=scratch, env=git_env)
        (main_clone / "tracked.txt").write_text("fixture\n", encoding="utf-8")
        git(["git", "-C", str(main_clone), "add", "tracked.txt"], cwd=scratch, env=git_env)
        git(["git", "-C", str(main_clone), "commit", "-m", "fixture"], cwd=scratch, env=git_env)
        for slug in ("foo", "foo-codex", "bar", "late"):
            git(
                [
                    "git",
                    "-C",
                    str(main_clone),
                    "worktree",
                    "add",
                    "-b",
                    f"fixture/{slug}",
                    str(worktree_root / slug),
                    "main",
                ],
                cwd=scratch,
                env=git_env,
            )

        pid_dir = scratch / "server-pids"
        pid_dir.mkdir()
        listener_script = scratch / "listener.py"
        listener_script.write_text(
            "import json, pathlib, signal, socket, sys, time\n"
            "record = pathlib.Path(sys.argv[2])\n"
            "server = socket.socket()\n"
            "server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)\n"
            "server.bind(('127.0.0.1', int(sys.argv[1])))\n"
            "server.listen()\n"
            "record.write_text(json.dumps({'pid': __import__('os').getpid(), 'port': server.getsockname()[1]}))\n"
            "while True:\n"
            "    try: client, _ = server.accept()\n"
            "    except OSError: break\n"
            "    client.close()\n",
            encoding="utf-8",
        )

        dev_server = scratch / "dev-server.py"
        dev_server.write_text(
            "import http.server, json, pathlib, sys\n"
            "port = int(sys.argv[1])\n"
            "pathlib.Path(sys.argv[2]).write_text(json.dumps({'pid': __import__('os').getpid(), 'port': port}))\n"
            "class Handler(http.server.BaseHTTPRequestHandler):\n"
            "    def do_GET(self): self.send_response(200); self.end_headers(); self.wfile.write(b'ok')\n"
            "    def log_message(self, *args): pass\n"
            "http.server.ThreadingHTTPServer(('127.0.0.1', port), Handler).serve_forever()\n",
            encoding="utf-8",
        )
        health_script = (
            "import socket; s=socket.create_connection(('127.0.0.1', {{port}}), timeout=1); s.close()"
        )
        settings = [
            "[paths]",
            f"main_clone = {toml_string(str(main_clone))}",
            f"worktree_root = {toml_string(str(worktree_root))}",
            f"state_db = {toml_string(str(state_db))}",
            f"cache_db = {toml_string(str(cache_root / 'cache.sqlite'))}",
            f"lock_dir = {toml_string(str(lock_dir))}",
            f"log_dir = {toml_string(str(log_dir))}",
            f"app_log_dir = {toml_string(str(scratch / 'app-logs'))}",
            f"dotfiles = {toml_string(str(scratch / 'dotfiles'))}",
            "[branch]",
            'base = "main"',
            'prefix = "fixture/"',
            "[tmux]",
            f"socket = {toml_string(socket_name)}",
            "[dev_server]",
            "command = "
            + toml_string(
                f"{shlex.quote(sys.executable)} {shlex.quote(str(dev_server))} "
                f"{{{{port}}}} {shlex.quote(str(pid_dir))}/dev-{{{{slug}}}}.json"
            ),
            "port_base = 39200",
            "port_range = 96",
            'url = "http://127.0.0.1:{{port}}/"',
            "max_concurrent = 2",
            "health_command = "
            + toml_string(f"{shlex.quote(sys.executable)} -c {shlex.quote(health_script)}"),
        ]
        config.write_text("\n".join(settings) + "\n", encoding="utf-8")

        fake_bin = scratch / "bin"
        fake_bin.mkdir()
        calls = scratch / "browser-calls.jsonl"
        browser = fake_bin / "browser-control"
        browser.write_text(
            "#!" + sys.executable + "\n"
            "import json, os, pathlib, sys\n"
            f"calls = pathlib.Path({str(calls)!r})\n"
            "argv = sys.argv[1:]\n"
            "with calls.open('a') as stream: stream.write(json.dumps(argv) + '\\n')\n"
            "if argv == ['status', '--json']:\n"
            "    port = os.environ.get('FIXTURE_DEV_PORT', '0')\n"
            "    print(json.dumps({'relay': {'running': True}, 'extension': {'sessions': ["
            "{'id': 'wt-foo', 'pageUrl': 'https://fixture.invalid/'},"
            "{'id': 'wt-foo-neighbor', 'pageUrl': 'https://fixture.invalid/'},"
            "{'id': 'foo-dev-tab', 'pageUrl': 'http://localhost:' + port + '/app'},"
            "{'id': 'unrelated-port', 'pageUrl': 'http://localhost:1/'},"
            "{'id': 'wt-bar', 'pageUrl': 'https://fixture.invalid/'}]}}))\n"
            "elif len(argv) == 3 and argv[:2] == ['session', 'delete']:\n"
            "    pass\n"
            "else:\n"
            "    raise SystemExit('unexpected browser-control invocation: ' + repr(argv))\n",
            encoding="utf-8",
        )
        browser.chmod(0o700)
        real_git = shutil.which("git")
        assert real_git
        late_path = worktree_root / "late"
        late_marker = scratch / "late-file-created"
        ls_count_path = scratch / "late-ls-count"
        git_trace = scratch / "git-wrapper-trace.txt"
        git_wrapper = fake_bin / "git"
        git_wrapper.write_text(
            "#!" + sys.executable + "\n"
            "import os, pathlib, subprocess, sys\n"
            "args = sys.argv[1:]\n"
            f"result = subprocess.run([{real_git!r}, *args], capture_output=True)\n"
            "sys.stdout.buffer.write(result.stdout)\n"
            "sys.stderr.buffer.write(result.stderr)\n"
            f"cwd = pathlib.Path(os.getcwd()).resolve()\n"
            f"target = pathlib.Path({str(late_path)!r}).resolve()\n"
            f"marker = pathlib.Path({str(late_marker)!r})\n"
            f"counter_path = pathlib.Path({str(ls_count_path)!r})\n"
            f"with pathlib.Path({str(git_trace)!r}).open('a') as trace: trace.write(repr((str(cwd), args)) + '\\n')\n"
            "if result.returncode == 0 and cwd == target and args == ['ls-files', '--others', '--exclude-standard', '-z']:\n"
            "    count = int(counter_path.read_text()) if counter_path.exists() else 0\n"
            "    count += 1\n"
            "    counter_path.write_text(str(count))\n"
            "    if count == 2:\n"
            "        marker.touch()\n"
            "        (target / 'arrived-after-snapshot.txt').write_text('new work\\n')\n"
            "raise SystemExit(result.returncode)\n",
            encoding="utf-8",
        )
        git_wrapper.chmod(0o700)
        fake_ps = fake_bin / "ps"
        real_ps = shutil.which("ps") or "/bin/ps"
        fake_ps.write_text(
            "#!" + sys.executable + "\n"
            "import os, sys\n"
            "if sys.argv[1:] == ['-Aco', 'command']:\n"
            "    print('COMMAND')\n"
            "else:\n"
            f"    os.execv({real_ps!r}, [{real_ps!r}, *sys.argv[1:]])\n",
            encoding="utf-8",
        )
        fake_ps.chmod(0o700)

        env = dict(os.environ)
        for key in list(env):
            if key.startswith("WT_") or key in {
                "TMUX",
                "TMUX_PANE",
                "XDG_CONFIG_HOME",
                "XDG_CACHE_HOME",
                "XDG_DATA_HOME",
                "XDG_STATE_HOME",
                "GIT_DIR",
                "GIT_WORK_TREE",
                "GIT_INDEX_FILE",
                "BUN_INSPECT",
                "BUN_OPTIONS",
                "NODE_OPTIONS",
            }:
                env.pop(key, None)
        env.update(
            {
                "PATH": f"{fake_bin}{os.pathsep}{env.get('PATH', '')}",
                "HOME": str(home),
                "SHELL": "/bin/bash",
                "XDG_CONFIG_HOME": str(home / ".config"),
                "XDG_CACHE_HOME": str(cache_root),
                "XDG_DATA_HOME": str(home / ".local/share"),
                "XDG_STATE_HOME": str(home / ".local/state"),
                "TMUX_TMPDIR": str(tmux_tmp),
                "GIT_CONFIG_NOSYSTEM": "1",
                "GIT_CONFIG_GLOBAL": str(git_env["GIT_CONFIG_GLOBAL"]),
                "WT_REPO_CONFIG": str(config),
                "WT_UPDATE": "off",
                "WT_SKILLS": "off",
                "WT_GITHUB": "off",
                "WT_AUTOMATIONS": "off",
                "FIXTURE_DEV_PORT": "0",
            }
        )
        fixture_processes: list[subprocess.Popen[str]] = []
        try:
            # A suffix-colliding session is owned by foo-codex, not foo.
            tmux(env, socket_name, "new-session", "-d", "-s", "foo-shell", "-c", str(worktree_root / "foo"), "sleep", "600")
            tmux(env, socket_name, "new-session", "-d", "-s", "foo-codex", "-c", str(worktree_root / "foo-codex"), "sleep", "600")
            tmux(env, socket_name, "new-session", "-d", "-s", "late-shell", "-c", str(worktree_root / "late"), "sleep", "600")
            direct_listener = start_listener(
                listener_script, worktree_root / "foo", pid_dir / "foo-listener.json", env
            )
            fixture_processes.append(direct_listener)
            neighbor_listener = start_listener(
                listener_script, worktree_root / "foo-codex", pid_dir / "neighbor-listener.json", env
            )
            fixture_processes.append(neighbor_listener)

            # A dirty checkout refuses removal before the locked cleanup hook.
            (worktree_root / "foo" / "uncommitted.txt").write_text("keep me\n", encoding="utf-8")
            refused = run([str(binary), "rm", "--yes", "foo"], cwd=main_clone, env=env, expected=1)
            assert "dirty" in refused.stderr.lower() or "uncommitted" in refused.stderr.lower(), refused.stderr
            assert direct_listener.poll() is None, "safety refusal killed the target listener"
            assert neighbor_listener.poll() is None, "safety refusal killed the neighboring listener"
            assert_session(env, socket_name, "foo-shell", True)
            assert_session(env, socket_name, "foo-codex", True)
            assert not calls.exists(), "safety refusal ran post-removal browser cleanup"

            # The Git shim adds an untracked file immediately after the
            # removal planner enumerates untracked paths. The locked revision
            # check must refuse before touching either owned resource.
            late_listener = start_listener(
                listener_script, late_path, pid_dir / "late-listener.json", env
            )
            fixture_processes.append(late_listener)
            changed = subprocess.run(
                [str(binary), "rm", "--yes", "--force", "late"],
                cwd=main_clone,
                env=env,
                text=True,
                capture_output=True,
                timeout=30,
            )
            assert changed.returncode == 1, (
                f"stale checkout removal exited {changed.returncode}; marker={late_marker.exists()}; "
                f"trace={git_trace.read_text() if git_trace.exists() else '<none>'}; "
                f"stdout={changed.stdout!r}; stderr={changed.stderr!r}"
            )
            assert late_marker.exists(), "fixture did not inject the post-snapshot file"
            assert "changed after the removal warning" in changed.stderr, changed.stderr
            assert late_listener.poll() is None, "stale revision refusal killed its listener"
            assert_session(env, socket_name, "late-shell", True)
            assert (late_path / "arrived-after-snapshot.txt").is_file()

            # Start a real private dev session to persist the port used by
            # browser cleanup, while all browser-control calls remain faked.
            run([str(binary), "dev", "start", "foo", "--wait", "--timeout", "20"], cwd=main_clone, env=env, timeout=35)
            status = json.loads(
                run([str(binary), "dev", "status", "foo", "--json"], cwd=main_clone, env=env).stdout
            )
            port = status["status"]["port"]
            assert isinstance(port, int) and port > 0, status
            env["FIXTURE_DEV_PORT"] = str(port)
            dev_pids = listener_pids(port)
            assert dev_pids, f"no owned process is listening on dev port {port}"

            removed = run(
                [str(binary), "rm", "--yes", "--force", "foo"],
                cwd=main_clone,
                env=env,
                timeout=45,
            )
            assert "removed foo" in removed.stdout, removed.stdout
            assert not (worktree_root / "foo").exists(), "successful removal left the target checkout"
            wait_exit(direct_listener, "foo listener")
            assert neighbor_listener.poll() is None, "cleanup killed foo-codex's listener"
            assert_session(env, socket_name, "foo-shell", False)
            assert_session(env, socket_name, "foo-dev", False)
            assert_session(env, socket_name, "foo-codex", True)
            assert_pids_reaped(dev_pids, real_ps)
            browser_calls = [json.loads(line) for line in calls.read_text().splitlines()]
            deleted = [call[2] for call in browser_calls if call[:2] == ["session", "delete"]]
            assert deleted == ["wt-foo", "foo-dev-tab"], deleted
            assert ["status", "--json"] in browser_calls
            assert "wt-foo-neighbor" not in deleted and "unrelated-port" not in deleted

            # Background destroy must pass through the same locked hook.
            tmux(env, socket_name, "new-session", "-d", "-s", "bar-shell", "-c", str(worktree_root / "bar"), "sleep", "600")
            background_listener = start_listener(
                listener_script, worktree_root / "bar", pid_dir / "bar-listener.json", env
            )
            fixture_processes.append(background_listener)
            queued = run(
                [str(binary), "rm", "--yes", "--force", "--background", "bar"],
                cwd=main_clone,
                env=env,
                timeout=20,
            )
            jobs = lock_dir / "destroy-jobs"
            deadline = time.monotonic() + 15
            job = None
            while time.monotonic() < deadline:
                records = list(jobs.glob("*.json")) if jobs.exists() else []
                for record in records:
                    current = json.loads(record.read_text(encoding="utf-8"))
                    if current.get("operation") == "remove" and current.get("target", {}).get("branch") == "fixture/bar":
                        job = current
                        break
                if job and job.get("state") in {"succeeded", "failed"}:
                    break
                time.sleep(0.05)
            assert job and job.get("state") == "succeeded", (queued.stdout, job)
            assert not (worktree_root / "bar").exists(), "background destroy left checkout"
            wait_exit(background_listener, "background bar listener")
            assert_session(env, socket_name, "bar-shell", False)
            background_calls = [json.loads(line) for line in calls.read_text().splitlines()]
            assert ["session", "delete", "wt-bar"] in background_calls
            print(
                "native cleanup checks passed: dirty and stale-revision refusals preserved owned resources; "
                "foreground and background removals reaped exact checkout resources; "
                "suffix neighbor and unrelated browser sessions survived"
            )
        finally:
            for process in fixture_processes:
                if process.poll() is None:
                    process.send_signal(signal.SIGTERM)
                    try:
                        process.wait(timeout=3)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=3)
            subprocess.run(["tmux", "-L", socket_name, "kill-server"], env=env, capture_output=True)
            shutil.rmtree(tmux_tmp, ignore_errors=True)


if __name__ == "__main__":
    main()
