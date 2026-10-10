#!/usr/bin/env python3
"""Exercise the native shared-host line protocol in isolated repositories."""

from __future__ import annotations

import argparse
import json
import os
import importlib.util
from pathlib import Path
import select
import shlex
import shutil
import signal
import subprocess
import sys
import tempfile
import time
from typing import Any


ROOT = Path(__file__).resolve().parents[1]
PROTOCOL = 1


def run(argv: list[str], *, cwd: Path, env: dict[str, str], timeout: float = 20) -> str:
    result = subprocess.run(
        argv, cwd=cwd, env=env, text=True, capture_output=True, timeout=timeout
    )
    if result.returncode:
        raise AssertionError(
            f"{argv!r} exited {result.returncode}\nstdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result.stdout


def git(argv: list[str], *, cwd: Path, env: dict[str, str]) -> None:
    subprocess.run(argv, cwd=cwd, env=env, check=True, capture_output=True, timeout=20)


def toml_string(value: str) -> str:
    return json.dumps(value)


def make_host(root: Path, name: str, binary: Path, token: str) -> dict[str, Any]:
    host_root = root / name
    home = host_root / "home"
    main = host_root / "main"
    worktree_root = host_root / "worktrees"
    for path in (home, main.parent, worktree_root):
        path.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, HOME=str(home), GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL="/dev/null")
    for key in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE"):
        env.pop(key, None)
    for key in ("TMUX", "TMUX_PANE"):
        env.pop(key, None)
    git(["git", "init", "-b", "main", str(main)], cwd=host_root, env=env)
    git(["git", "-C", str(main), "config", "user.name", "native host fixture"], cwd=host_root, env=env)
    git(
        ["git", "-C", str(main), "config", "user.email", "host-fixture@example.invalid"],
        cwd=host_root,
        env=env,
    )
    (main / "fixture.txt").write_text(f"isolated repository {name}\n", encoding="utf-8")
    git(["git", "-C", str(main), "add", "fixture.txt"], cwd=host_root, env=env)
    git(["git", "-C", str(main), "commit", "-m", "fixture"], cwd=host_root, env=env)
    checkout = worktree_root / "same-slug"
    git(
        ["git", "-C", str(main), "worktree", "add", "-b", "fixture/same-slug", str(checkout), "main"],
        cwd=host_root,
        env=env,
    )

    socket_name = f"wt-host-check-{token}-{name}"
    config = main / ".wt.toml"
    config.write_text(
        "\n".join(
            [
                "[paths]",
                f"main_clone = {toml_string(str(main))}",
                f"worktree_root = {toml_string(str(worktree_root))}",
                f"cache_db = {toml_string(str(host_root / 'cache.sqlite'))}",
                f"state_db = {toml_string(str(host_root / 'state.sqlite'))}",
                f"log_dir = {toml_string(str(host_root / 'logs'))}",
                f"lock_dir = {toml_string(str(host_root / 'locks'))}",
                "",
                "[branch]",
                "prefix = 'fixture'",
                "base = 'main'",
                "",
                "[instance]",
                "role = 'worker'",
                "",
                "[tmux]",
                f"socket = {toml_string(socket_name)}",
                "",
            ]
        ),
        encoding="utf-8",
    )
    env.update(
        {
            "WT_CONFIG": str(config),
            "WT_REPO_CONFIG": str(config),
            "XDG_CONFIG_HOME": str(home / "xdg-config"),
            "XDG_CACHE_HOME": str(home / "xdg-cache"),
            "XDG_STATE_HOME": str(home / "xdg-state"),
            "WT_UPDATE": "off",
            "WT_SKILLS": "off",
            "WT_GITHUB": "off",
            "WT_AUTOMATIONS": "off",
            "GIT_TERMINAL_PROMPT": "0",
        }
    )
    for directory in ("XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME"):
        Path(env[directory]).mkdir(parents=True, exist_ok=True)
    return {
        "name": name,
        "root": host_root,
        "home": home,
        "main": main,
        "checkout": checkout,
        "config": config,
        "env": env,
        "socket": socket_name,
        "binary": binary,
        "stderr": host_root / "host.stderr.log",
    }


class HostProcess:
    def __init__(self, host: dict[str, Any]):
        self.host = host
        self.stderr_file = host["stderr"].open("wb")
        self.process = subprocess.Popen(
            [str(host["binary"]), "_host"],
            cwd=host["main"],
            env=host["env"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.stderr_file,
            start_new_session=True,
            bufsize=0,
        )
        assert self.process.stdin is not None and self.process.stdout is not None
        self.input = self.process.stdin
        self.output = self.process.stdout
        self.buffer = bytearray()

    def stderr(self) -> str:
        self.stderr_file.flush()
        return self.host["stderr"].read_text(errors="replace")[-8000:]

    def send(self, frame: dict[str, Any], *, fragmented: bool = False) -> None:
        payload = json.dumps(frame, separators=(",", ":")).encode() + b"\n"
        if fragmented:
            cuts = (1, 4, 11, max(12, len(payload) // 2), len(payload))
            start = 0
            for end in cuts:
                end = min(end, len(payload))
                if end > start:
                    self.input.write(payload[start:end])
                    self.input.flush()
                    time.sleep(0.003)
                    start = end
            if start < len(payload):
                self.input.write(payload[start:])
                self.input.flush()
        else:
            self.input.write(payload)
            self.input.flush()

    def read(self, timeout: float = 10) -> dict[str, Any]:
        deadline = time.monotonic() + timeout
        while b"\n" not in self.buffer:
            if self.process.poll() is not None:
                raise AssertionError(
                    f"host {self.host['name']} exited {self.process.returncode}: {self.stderr()}"
                )
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise TimeoutError(f"timed out reading host {self.host['name']}: {self.stderr()}")
            ready, _, _ = select.select([self.output], [], [], remaining)
            if not ready:
                continue
            chunk = os.read(self.output.fileno(), 64 * 1024)
            if not chunk:
                raise EOFError(f"host {self.host['name']} closed stdout: {self.stderr()}")
            self.buffer.extend(chunk)
            if len(self.buffer) > 8 * 1024 * 1024:
                raise AssertionError("host protocol frame exceeded the fixture's 8 MiB bound")
        line, _, rest = self.buffer.partition(b"\n")
        self.buffer[:] = rest
        return json.loads(line)

    def read_available(self, timeout: float) -> list[dict[str, Any]]:
        frames: list[dict[str, Any]] = []
        deadline = time.monotonic() + timeout
        while True:
            if b"\n" in self.buffer:
                line, _, rest = self.buffer.partition(b"\n")
                self.buffer[:] = rest
                frames.append(json.loads(line))
                continue
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return frames
            ready, _, _ = select.select([self.output], [], [], remaining)
            if not ready:
                return frames
            chunk = os.read(self.output.fileno(), 64 * 1024)
            if not chunk:
                return frames
            self.buffer.extend(chunk)

    def hello(self, *, protocol: int = PROTOCOL, fragmented: bool = False) -> dict[str, Any]:
        self.send({"Hello": {"protocol": protocol}}, fragmented=fragmented)
        return self.read()

    def wait(self, predicate, timeout: float = 12) -> dict[str, Any]:
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            frame = self.read(max(0.05, deadline - time.monotonic()))
            if predicate(frame):
                return frame
        raise TimeoutError(f"host {self.host['name']} did not publish the expected frame")

    def command(self, command_id: int, action: dict[str, Any], *, fragmented: bool = False) -> dict[str, Any]:
        self.send({"Command": {"id": command_id, "action": action}}, fragmented=fragmented)
        return self.wait(lambda frame: "Reply" in frame and frame["Reply"].get("id") == command_id)

    def close(self) -> None:
        if self.process.poll() is None:
            try:
                self.input.close()
            except OSError:
                pass
            try:
                self.process.wait(timeout=8)
            except subprocess.TimeoutExpired:
                os.killpg(self.process.pid, signal.SIGTERM)
                try:
                    self.process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    os.killpg(self.process.pid, signal.SIGKILL)
                    self.process.wait(timeout=3)
        self.stderr_file.close()


def snapshot_frame(frame: dict[str, Any]) -> dict[str, Any] | None:
    value = frame.get("Snapshot")
    if not isinstance(value, dict):
        return None
    return value


def row(snapshot: dict[str, Any], key: str) -> dict[str, Any] | None:
    board = snapshot.get("board")
    rows = board.get("rows") if isinstance(board, dict) else None
    if not isinstance(rows, list):
        return None
    return next((item for item in rows if isinstance(item, dict) and item.get("key") == key), None)


def wait_snapshot(host: HostProcess, predicate, timeout: float = 12) -> dict[str, Any]:
    frame = host.wait(lambda value: (snap := snapshot_frame(value)) is not None and predicate(snap), timeout)
    return frame["Snapshot"]


def send_title(host: HostProcess, command_id: int, title: str, *, fragmented: bool = False) -> None:
    reply = host.command(
        command_id,
        {"SetTitle": {"key": "same-slug", "title": title}},
        fragmented=fragmented,
    )
    result = reply["Reply"]["reply"]
    assert not result["failed"], result
    snapshot = wait_snapshot(
        host,
        lambda value: row(value, "same-slug") is not None
        and row(value, "same-slug").get("title") == title,
    )
    assert row(snapshot, "same-slug")["title"] == title


def check_mismatched_protocol(binary: Path, host: dict[str, Any]) -> None:
    process = subprocess.Popen(
        [str(binary), "_host"],
        cwd=host["main"],
        env=host["env"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )
    assert process.stdin is not None and process.stdout is not None and process.stderr is not None
    process.stdin.write(json.dumps({"Hello": {"protocol": PROTOCOL + 1}}).encode() + b"\n")
    process.stdin.flush()
    try:
        code = process.wait(timeout=8)
    except subprocess.TimeoutExpired:
        os.killpg(process.pid, signal.SIGTERM)
        process.wait(timeout=3)
        raise AssertionError("host did not reject a mismatched protocol")
    stdout = process.stdout.read().decode(errors="replace")
    stderr = process.stderr.read().decode(errors="replace")
    assert code != 0, (stdout, stderr)
    assert not stdout.strip(), stdout
    assert "protocol mismatch" in stderr.lower(), stderr


def check_active_action_survives_remote_runtime_upgrade(
    host: dict[str, Any], root: Path, binary: Path, owned_servers: list[subprocess.Popen]
) -> None:
    tmux = shutil.which("tmux")
    if not tmux:
        raise RuntimeError("tmux is required for the native host fixture")
    run_id = "fixture0001"
    run_dir = host["root"] / "logs" / "actions" / run_id
    run_dir.mkdir(parents=True)
    run_dir = run_dir.resolve()
    pid_file = root / f"{host['name']}-action-child.pid"
    action_command = [
        "/bin/sh",
        "-c",
        f"printf 'native-host-action-ready\\n'; echo $$ > {shlex.quote(str(pid_file))}; exec sleep 120",
    ]
    meta = {
        "version": 1,
        "slug": "same-slug",
        "runId": run_id,
        "actionKey": "same-slug",
        "kind": "shell",
        "actionId": "fixture",
        "actionName": "Native host fixture action",
        "prompt": "",
        "affects": [],
        "startedAt": 1,
        "status": "ambiguous",
        "extra": {},
    }
    job = {
        "version": 1,
        "request": {
            "actionKey": "same-slug",
            "slug": "same-slug",
            "actionId": "fixture",
            "actionName": "Native host fixture action",
            "prompt": "",
            "kind": "shell",
            "command": action_command,
            "cwd": str(host["checkout"]),
            "affects": [],
            "configSelectors": {
                "WT_CONFIG": str(host["config"]),
                "WT_REPO_CONFIG": str(host["config"]),
            },
        },
        "run": {
            "meta": meta,
            "runDir": str(run_dir),
            "command": action_command,
            "cwd": str(host["checkout"]),
        },
        "session": "native-host-active-action",
    }
    (run_dir / "job.json").write_text(json.dumps(job), encoding="utf-8")
    (run_dir / "meta.json").write_text(json.dumps(meta), encoding="utf-8")

    worker_command = shlex.join(
        [str(binary), "_action-worker", "--job", str(run_dir / "job.json")]
    )
    command = shlex.join(
        [
            "/bin/sh",
            "-c",
            f"{worker_command}; worker_rc=$?; printf 'worker-exit=%s\\n' \"$worker_rc\"; sleep 120",
        ]
    )
    run(
        [tmux, "-L", host["socket"], "new-session", "-d", "-s", job["session"], command],
        cwd=host["main"],
        env=host["env"],
    )
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        if pid_file.is_file() and (run_dir / "stream.log").is_file():
            if "native-host-action-ready" in (run_dir / "stream.log").read_text(errors="replace"):
                break
        if (run_dir / "done.json").exists():
            raise AssertionError(f"fixture action ended before runtime upgrade: {(run_dir / 'done.json').read_text()}")
        time.sleep(0.05)
    else:
        pane = subprocess.run(
            [tmux, "-L", host["socket"], "capture-pane", "-pt", "native-host-active-action"],
            cwd=host["main"],
            env=host["env"],
            text=True,
            capture_output=True,
            timeout=5,
        )
        done = (run_dir / "done.json").read_text(errors="replace") if (run_dir / "done.json").exists() else "<missing>"
        meta_text = (run_dir / "meta.json").read_text(errors="replace") if (run_dir / "meta.json").exists() else "<missing>"
        raise TimeoutError(
            "supervised action did not start; "
            f"pane={pane.stdout!r} pane_error={pane.stderr!r} done={done!r} meta={meta_text!r}"
        )
    child_pid = int(pid_file.read_text(encoding="utf-8"))

    # Reuse the existing private-sshd fixture so this reaches the production
    # content-addressed upload, verification, and publication path.
    fixture_script = ROOT / "scripts/native-remote-check.py"
    spec = importlib.util.spec_from_file_location("native_remote_check", fixture_script)
    assert spec is not None and spec.loader is not None
    remote_fixture = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(remote_fixture)
    worker_home = host["home"]
    server, server_log = remote_fixture.start_isolated_sshd(root, host["config"], worker_home)
    if server is None:
        raise RuntimeError(f"private sshd is required for runtime-upgrade coverage: {server_log.read_text(errors='replace')}")
    owned_servers.append(server)

    controller_home = root / f"{host['name']}-upgrade-controller-home"
    controller_home.mkdir()
    controller_cwd = root / f"{host['name']}-upgrade-controller-cwd"
    controller_cwd.mkdir()
    controller_config = controller_home / "config.toml"
    controller_config.write_text(
        "\n".join(
            [
                "[paths]",
                f"main_clone = {toml_string(str(host['main']))}",
                f"worktree_root = {toml_string(str(host['checkout'].parent))}",
                f"cache_db = {toml_string(str(controller_home / 'cache.sqlite'))}",
                f"state_db = {toml_string(str(controller_home / 'state.sqlite'))}",
                "",
                "[branch]",
                "prefix = 'fixture'",
                "base = 'main'",
                "",
                "[remote]",
                "host = 'fixture-host'",
                "label = 'Fixture worker'",
                f"wt_path = {toml_string(str(binary))}",
                "",
            ]
        ),
        encoding="utf-8",
    )
    ssh_program = shutil.which("ssh")
    if not ssh_program:
        raise RuntimeError("ssh is required for native runtime-upgrade coverage")
    ssh_config = root / "ssh_config"
    ssh_output = run(
        [ssh_program, "-F", str(root / "ssh_config"), "-G", "fixture-host"],
        cwd=controller_cwd,
        env=os.environ.copy(),
    )
    assert "hostname 127.0.0.1\n" in ssh_output, ssh_output
    bin_dir = root / f"{host['name']}-ssh-bin"
    bin_dir.mkdir()
    ssh_wrapper = bin_dir / "ssh"
    ssh_wrapper.write_text(
        "#!" + sys.executable + "\n"
        "import os, sys\n"
        f"os.execve({ssh_program!r}, [{ssh_program!r}, '-F', {str(ssh_config)!r}, *sys.argv[1:]], os.environ)\n",
        encoding="utf-8",
    )
    ssh_wrapper.chmod(0o700)
    controller_env = dict(host["env"])
    for key in ("WT_REPO_CONFIG", "TMUX", "TMUX_PANE"):
        controller_env.pop(key, None)
    controller_env.update(
        {
            "HOME": str(controller_home),
            "WT_CONFIG": str(controller_config),
            "PATH": str(bin_dir) + os.pathsep + os.environ.get("PATH", ""),
            "XDG_CONFIG_HOME": str(controller_home / "xdg-config"),
            "XDG_CACHE_HOME": str(controller_home / "xdg-cache"),
            "XDG_STATE_HOME": str(controller_home / "xdg-state"),
        }
    )
    for key in ("XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME"):
        Path(controller_env[key]).mkdir(parents=True, exist_ok=True)

    candidates = root / f"{host['name']}-candidate-wt"
    candidates.mkdir()
    first = candidates / "wt-v1"
    second = candidates / "wt-v2"
    shutil.copy2(binary, first)
    shutil.copy2(binary, second)
    with second.open("ab") as upgraded:
        upgraded.write(b"fixture-runtime-revision-2\n")

    try:
        for candidate in (first, second):
            run([str(candidate), "remote", "version"], cwd=controller_cwd, env=controller_env)
            run(
                [tmux, "-L", host["socket"], "has-session", "-t", "=native-host-active-action"],
                cwd=host["main"],
                env=host["env"],
            )
            os.kill(child_pid, 0)
            assert not (run_dir / "done.json").exists(), "runtime upgrade terminated the active action"
            current_meta = json.loads((run_dir / "meta.json").read_text(encoding="utf-8"))
            assert current_meta["status"] == "running", current_meta
    finally:
        os.killpg(server.pid, signal.SIGTERM)
        server.wait(timeout=5)
        run([tmux, "-L", host["socket"], "kill-server"], cwd=host["main"], env=host["env"])


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/wt")
    args = parser.parse_args()
    binary = args.binary.resolve()
    if not binary.is_file():
        raise SystemExit(f"native wt binary not found: {binary}")

    token = str(os.getpid())
    with tempfile.TemporaryDirectory(prefix="wt-native-host-check-", dir="/tmp") as temporary:
        root = Path(temporary)
        hosts = [make_host(root, f"host-{index}", binary, token) for index in (1, 2)]
        processes: list[HostProcess] = []
        owned_servers: list[subprocess.Popen] = []
        tmux_sockets: set[str] = {host["socket"] for host in hosts}
        try:
            first, second = (HostProcess(host) for host in hosts)
            processes.extend([first, second])
            for host_process in (first, second):
                hello = host_process.hello(fragmented=True)
                assert "Hello" in hello and hello["Hello"]["protocol"] == PROTOCOL, hello
                assert hello["Hello"].get("build"), hello
                wait_snapshot(
                    host_process,
                    lambda value: value.get("state") in ("Ready", {"Ready": None})
                    and row(value, "same-slug") is not None,
                )

            # Both repositories contain the same slug, but writes stay inside
            # the selected host's independent state database.
            send_title(first, 1, "host one title", fragmented=True)
            status_reply = first.command(
                2,
                {
                    "SetStatus": {
                        "key": "same-slug",
                        "state": "working",
                        "note": "host-local status",
                        "verify_after_merge": None,
                    }
                },
                fragmented=True,
            )
            assert not status_reply["Reply"]["reply"]["failed"], status_reply
            status_snapshot = wait_snapshot(
                first,
                lambda value: value.get("layout", {}).get("same-slug", {}).get("work", {}).get("state")
                == "working",
            )
            assert status_snapshot["layout"]["same-slug"]["work"]["state"] == "working"

            prepare = first.command(3, {"PrepareRemove": {"key": "same-slug"}}, fragmented=True)
            prepare_reply = prepare["Reply"]["reply"]
            assert not prepare_reply["failed"], prepare_reply
            assert prepare_reply["modal"] is not None, prepare_reply
            assert hosts[0]["checkout"].is_dir(), "preparing removal must not delete a checkout"

            second_snapshot = wait_snapshot(
                second,
                lambda value: row(value, "same-slug") is not None,
            )
            assert row(second_snapshot, "same-slug").get("title") != "host one title"
            assert second_snapshot["layout"].get("same-slug", {}).get("work") is None

            # Disconnecting one endpoint cannot stop another worker's stream.
            first.close()
            processes.remove(first)
            send_title(second, 1, "host two survives", fragmented=True)
            assert hosts[1]["checkout"].is_dir()

            # A new connection receives durable state as a snapshot, never an
            # old command reply. The id counter starts fresh per connection.
            reconnected = HostProcess(hosts[0])
            processes.append(reconnected)
            hello = reconnected.hello()
            assert hello["Hello"]["protocol"] == PROTOCOL
            restored = wait_snapshot(
                reconnected,
                lambda value: row(value, "same-slug") is not None
                and row(value, "same-slug").get("title") == "host one title",
            )
            assert restored["layout"]["same-slug"]["work"]["state"] == "working"
            unsolicited = reconnected.read_available(0.25)
            assert all("Reply" not in frame for frame in unsolicited), (
                f"reconnect replayed an unsolicited command reply: {unsolicited!r}"
            )

            check_mismatched_protocol(binary, hosts[0])
            check_active_action_survives_remote_runtime_upgrade(hosts[0], root, binary, owned_servers)
            print(
                "native host checks passed: fragmented protocol, isolated same-slug state, "
                "title/status writes, non-destructive removal preflight, reconnect, endpoint loss, "
                "protocol rejection, and active-action survival through two remote runtime publications"
            )
        finally:
            for server in owned_servers:
                if server.poll() is None:
                    os.killpg(server.pid, signal.SIGTERM)
                    try:
                        server.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        os.killpg(server.pid, signal.SIGKILL)
                        server.wait(timeout=3)
            for process in processes:
                process.close()
            tmux = shutil.which("tmux")
            if tmux:
                for socket_name in tmux_sockets:
                    subprocess.run(
                        [tmux, "-L", socket_name, "kill-server"],
                        cwd=root,
                        env=hosts[0]["env"],
                        stdout=subprocess.DEVNULL,
                        stderr=subprocess.DEVNULL,
                        timeout=5,
                    )


if __name__ == "__main__":
    main()
