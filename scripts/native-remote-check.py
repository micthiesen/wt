#!/usr/bin/env python3
"""Exercise worker handshake, snapshot, native provisioning, and runtime reuse."""

from __future__ import annotations

import argparse
import getpass
import hashlib
import json
import os
from pathlib import Path
import shutil
import signal
import shlex
import socket
import subprocess
import sys
import tempfile
import time


ROOT = Path(__file__).resolve().parents[1]


def native_target() -> str:
    system = (
        "Darwin"
        if sys.platform == "darwin"
        else "Linux"
        if sys.platform.startswith("linux")
        else sys.platform
    )
    machine = os.uname().machine
    if (system, machine) in (("Darwin", "arm64"), ("Darwin", "aarch64")):
        return "aarch64-apple-darwin"
    if (system, machine) in (("Darwin", "x86_64"), ("Darwin", "amd64")):
        return "x86_64-apple-darwin"
    if (system, machine) in (("Linux", "aarch64"), ("Linux", "arm64")):
        return "aarch64-unknown-linux-gnu"
    if (system, machine) in (("Linux", "x86_64"), ("Linux", "amd64")):
        return "x86_64-unknown-linux-gnu"
    raise AssertionError(f"unsupported native fixture platform: {system} {machine}")


def run(argv: list[str], *, cwd: Path, env: dict[str, str], expected: int = 0) -> str:
    result = subprocess.run(argv, cwd=cwd, env=env, text=True, capture_output=True, timeout=30)
    if result.returncode != expected:
        raise AssertionError(
            f"{argv!r} exited {result.returncode}, expected {expected}\n"
            f"stdout:\n{result.stdout}\nstderr:\n{result.stderr}"
        )
    return result.stdout


def git(argv: list[str], *, cwd: Path, env: dict[str, str]) -> None:
    subprocess.run(argv, cwd=cwd, env=env, check=True, capture_output=True, timeout=20)


def start_isolated_sshd(
    root: Path, worker_config: Path, worker_home: Path
) -> tuple[subprocess.Popen, Path] | tuple[None, Path]:
    """Start an unprivileged, key-only SSH server; return None if unavailable."""
    log = root / "sshd.log"
    sshd = shutil.which("sshd")
    if not sshd:
        log.write_text("sshd is not installed")
        return None, log
    ssh_keygen = shutil.which("ssh-keygen")
    if not ssh_keygen:
        log.write_text("ssh-keygen is not installed")
        return None, log
    key = root / "host-key"
    identity = root / "client-key"
    subprocess.run([ssh_keygen, "-q", "-t", "ed25519", "-N", "", "-f", str(key)],
                   check=True, capture_output=True, timeout=10)
    subprocess.run([ssh_keygen, "-q", "-t", "ed25519", "-N", "", "-f", str(identity)],
                   check=True, capture_output=True, timeout=10)
    authorized_keys = root / "authorized_keys"
    authorized_keys.write_text(identity.with_suffix(".pub").read_text())
    authorized_keys.chmod(0o600)

    wrapper = root / "worker-command.py"
    command_log = root / "ssh-original-commands.jsonl"
    wrapper.write_text(
        "#!" + sys.executable + "\n"
        "import base64, json, os, shlex, subprocess, sys\n"
        f"home = {str(worker_home)!r}\n"
        f"log_path = {str(command_log)!r}\n"
        "command = os.environ.get('SSH_ORIGINAL_COMMAND', '')\n"
        "argv = shlex.split(command)\n"
        "record = {'command': command, 'argv': argv}\n"
        "if command.startswith(\"printf 'WT_REMOTE_PLATFORM\\\\n'; uname -s; uname -m\"):\n"
        "    record['kind'] = 'platform'\n"
        "elif command.startswith('set -eu; umask 077; dir='):\n"
        "    record['kind'] = 'upload'\n"
        "elif command.startswith('set -eu\\n'):\n"
        "    record['kind'] = 'publish' if 'stage=\"$dir/.wt-stage-' in command else 'cache-probe'\n"
        "elif ' _remote ' in command:\n"
        "    payload = argv[-1]\n"
        "    decoded = json.loads(base64.urlsafe_b64decode(payload + '=' * (-len(payload) % 4)))\n"
        "    if decoded not in (['_hello'], ['version']): raise SystemExit('unexpected encoded worker argv')\n"
        "    record.update(kind='remote', decoded=decoded)\n"
        "else:\n"
        "    raise SystemExit('unexpected remote command')\n"
        "with open(log_path, 'a') as log: log.write(json.dumps(record) + '\\n')\n"
        "env = dict(os.environ, HOME=home)\n"
        "if record['kind'] == 'remote': os.execve('/bin/sh', ['/bin/sh', '-c', command], env)\n"
        "result = subprocess.run(['/bin/sh', '-c', command], env=env)\n"
        "raise SystemExit(result.returncode)\n"
    )
    wrapper.chmod(0o700)

    with socket.socket() as listener:
        listener.bind(("127.0.0.1", 0))
        port = listener.getsockname()[1]
    sshd_config = root / "sshd_config"
    xdg = root / "worker-xdg"
    for directory in (xdg / "config", xdg / "cache", xdg / "state"):
        directory.mkdir(parents=True)
    sshd_config.write_text(
        "\n".join(
            [
                "AddressFamily inet",
                "ListenAddress 127.0.0.1",
                f"Port {port}",
                f"HostKey {key}",
                f"PidFile {root / 'sshd.pid'}",
                f"AuthorizedKeysFile {authorized_keys}",
                f"AllowUsers {getpass.getuser()}",
                "PubkeyAuthentication yes",
                "AuthenticationMethods publickey",
                "PasswordAuthentication no",
                "KbdInteractiveAuthentication no",
                "PermitRootLogin no",
                "StrictModes no",
                "UsePAM no",
                "PermitUserEnvironment no",
                "PrintMotd no",
                "PrintLastLog no",
                "LogLevel ERROR",
                f"ForceCommand {wrapper}",
                "SetEnv "
                + " ".join(
                    [
                        f"WT_CONFIG={worker_config}",
                        f"XDG_CONFIG_HOME={xdg / 'config'}",
                        f"XDG_CACHE_HOME={xdg / 'cache'}",
                        f"XDG_STATE_HOME={xdg / 'state'}",
                        "WT_UPDATE=off",
                        "WT_SKILLS=off",
                        "WT_GITHUB=off",
                        "WT_AUTOMATIONS=off",
                        "GIT_CONFIG_NOSYSTEM=1",
                        "GIT_CONFIG_GLOBAL=/dev/null",
                    ]
                ),
                "",
            ]
        )
    )
    config_check = subprocess.run([sshd, "-t", "-f", str(sshd_config)],
                                  text=True, capture_output=True, timeout=10)
    if config_check.returncode:
        log.write_text(config_check.stderr or config_check.stdout)
        return None, log
    log_stream = log.open("wb")
    process = subprocess.Popen(
        [sshd, "-D", "-e", "-f", str(sshd_config)],
        stdout=subprocess.DEVNULL,
        stderr=log_stream,
        start_new_session=True,
    )
    log_stream.close()
    ready = False
    deadline = time.monotonic() + 5
    while time.monotonic() < deadline:
        if process.poll() is not None:
            log.write_text(log.read_text(errors="replace") + "\nsshd exited before listening")
            return None, log
        try:
            with socket.create_connection(("127.0.0.1", port), timeout=0.1):
                ready = True
                break
        except OSError:
            time.sleep(0.05)
    if not ready:
        os.killpg(process.pid, signal.SIGTERM)
        process.wait(timeout=5)
        log.write_text(log.read_text(errors="replace") + "\nsshd did not listen within 5 seconds")
        return None, log

    ssh_config = root / "ssh_config"
    ssh_config.write_text(
        "\n".join(
            [
                "Host fixture-host",
                "  HostName 127.0.0.1",
                f"  Port {port}",
                f"  User {getpass.getuser()}",
                f"  IdentityFile {identity}",
                "  IdentitiesOnly yes",
                "  StrictHostKeyChecking no",
                "  UserKnownHostsFile /dev/null",
                "  LogLevel ERROR",
                "  ProxyCommand none",
                "  ProxyJump none",
                "",
            ]
        )
    )
    ssh_config.chmod(0o600)
    return process, command_log


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/wt")
    args = parser.parse_args()
    binary = args.binary.resolve()
    if not binary.is_file():
        raise SystemExit(f"native wt binary not found: {binary}")

    with tempfile.TemporaryDirectory(prefix="wt-native-remote-check-", dir="/tmp") as temporary:
        root = Path(temporary)
        worker_home = root / "worker-home"
        controller_home = root / "controller-home"
        worker_home.mkdir()
        controller_home.mkdir()
        main_clone = root / "main-clone"
        worktree_root = root / "worktrees"
        worktree_root.mkdir()
        origin = root / "origin.git"
        git_env = dict(os.environ, HOME=str(worker_home), GIT_CONFIG_NOSYSTEM="1")
        for name in ("GIT_DIR", "GIT_WORK_TREE", "GIT_INDEX_FILE", "GIT_CONFIG_GLOBAL"):
            git_env.pop(name, None)
        git(["git", "init", "--bare", "--initial-branch=main", str(origin)], cwd=root, env=git_env)
        git(["git", "clone", str(origin), str(main_clone)], cwd=root, env=git_env)
        git(["git", "config", "user.name", "native remote fixture"], cwd=main_clone, env=git_env)
        git(["git", "config", "user.email", "remote-fixture@example.invalid"], cwd=main_clone, env=git_env)
        (main_clone / "fixture.txt").write_text("native worker fixture\n")
        git(["git", "add", "fixture.txt"], cwd=main_clone, env=git_env)
        git(["git", "commit", "-m", "fixture"], cwd=main_clone, env=git_env)
        git(["git", "push", "origin", "main"], cwd=main_clone, env=git_env)
        branch = "fixture/remote-check"
        worktree = worktree_root / "remote-check"
        git(["git", "worktree", "add", "-b", branch, str(worktree)], cwd=main_clone, env=git_env)

        worker_config = root / "worker.toml"
        worker_config.write_text(
            "\n".join(
                [
                    "[instance]",
                    'role = "worker"',
                    "[paths]",
                    f'main_clone = {json.dumps(str(main_clone))}',
                    f'worktree_root = {json.dumps(str(worktree_root))}',
                    f'cache_db = {json.dumps(str(root / "worker-cache.sqlite"))}',
                    f'state_db = {json.dumps(str(root / "worker-state.sqlite"))}',
                    "[branch]",
                    'prefix = "fixture"',
                    'base = "main"',
                    "[naming]",
                    "auto_rename = false",
                    "",
                ]
            )
        )
        controller_config = root / "controller.toml"
        controller_config.write_text(
            "\n".join(
                [
                    "[paths]",
                    f'main_clone = {json.dumps(str(main_clone))}',
                    f'worktree_root = {json.dumps(str(worktree_root))}',
                    f'cache_db = {json.dumps(str(root / "controller-cache.sqlite"))}',
                    f'state_db = {json.dumps(str(root / "controller-state.sqlite"))}',
                    "[branch]",
                    'prefix = "fixture"',
                    'base = "main"',
                    "[naming]",
                    "auto_rename = false",
                    "[remote]",
                    'host = "fixture-host"',
                    'label = "Fixture"',
                    f'wt_path = {json.dumps(str(binary))}',
                    "",
                ]
            )
        )

        server, actual_command_log = start_isolated_sshd(root, worker_config, worker_home)
        transport = "isolated OpenSSH server"
        fallback_reason = None
        ssh_log = root / "ssh-argv.jsonl"
        fake_bin = root / "bin"
        fake_bin.mkdir()
        if server is None:
            transport = "fake ssh (isolated OpenSSH unavailable)"
            fallback_reason = actual_command_log.read_text(errors="replace")
            fake_ssh = fake_bin / "ssh"
            actual_command_log = root / "fake-worker-command.jsonl"
            fake_ssh.write_text(
                "#!" + sys.executable + "\n"
                "import base64, json, os, shlex, subprocess, sys\n"
                f"with open({str(ssh_log)!r}, 'a') as log: log.write(json.dumps(sys.argv[1:]) + '\\n')\n"
                f"command = sys.argv[-1]; argv = shlex.split(command); log_path = {str(actual_command_log)!r}\n"
                "record = {'command': command, 'argv': argv}\n"
                "if 'WT_REMOTE_PLATFORM' in command: record['kind'] = 'platform'\n"
                "elif command.startswith('set -eu; umask 077;'): record['kind'] = 'upload'\n"
                "elif command.startswith('set -eu\\n'): record['kind'] = 'publish' if 'stage=\"$dir/.wt-stage-' in command else 'cache-probe'\n"
                "elif ' _remote ' in command:\n"
                " payload = argv[-1]; decoded = json.loads(base64.urlsafe_b64decode(payload + '=' * (-len(payload) % 4)))\n"
                " if decoded not in (['_hello'], ['version']): raise SystemExit('unexpected encoded worker argv')\n"
                " record.update(kind='remote', decoded=decoded)\n"
                "else: raise SystemExit('unexpected remote command')\n"
                "with open(log_path, 'a') as log: log.write(json.dumps(record) + '\\n')\n"
                f"env = dict(os.environ, HOME={str(worker_home)!r}, WT_CONFIG={str(worker_config)!r})\n"
                "result = subprocess.run(['/bin/sh', '-c', command], env=env)\n"
                "raise SystemExit(result.returncode)\n"
            )
            fake_ssh.chmod(0o755)
        else:
            real_ssh = shutil.which("ssh")
            assert real_ssh is not None
            ssh_wrapper = fake_bin / "ssh"
            ssh_config = root / "ssh_config"
            ssh_wrapper.write_text(
                "#!" + sys.executable + "\n"
                "import json, os, sys\n"
                f"with open({str(ssh_log)!r}, 'a') as log: log.write(json.dumps(sys.argv[1:]) + '\\n')\n"
                f"os.execve({real_ssh!r}, [{real_ssh!r}, '-F', {str(ssh_config)!r}, *sys.argv[1:]], os.environ)\n"
            )
            ssh_wrapper.chmod(0o755)
        env = dict(os.environ)
        for key in list(env):
            if key.startswith("WT_") or key in {
                "TMUX",
                "TMUX_PANE",
                "BUN_INSPECT",
                "XDG_CONFIG_HOME",
                "XDG_CACHE_HOME",
                "XDG_STATE_HOME",
                "GIT_DIR",
                "GIT_WORK_TREE",
                "GIT_INDEX_FILE",
                "GIT_CONFIG_GLOBAL",
            }:
                env.pop(key, None)
        env.update(
            {
                "HOME": str(controller_home),
                "WT_CONFIG": str(controller_config),
                "WT_UPDATE": "off",
                "WT_SKILLS": "off",
                "WT_GITHUB": "off",
                "WT_AUTOMATIONS": "off",
                "PATH": str(fake_bin) + os.pathsep + os.environ.get("PATH", ""),
            }
        )
        if server is not None:
            ssh_config = subprocess.run(
                [shutil.which("ssh"), "-F", str(root / "ssh_config"), "-G", "fixture-host"],
                cwd=main_clone,
                env=env,
                text=True,
                capture_output=True,
                timeout=10,
            )
            if ssh_config.returncode != 0 or "hostname 127.0.0.1\n" not in ssh_config.stdout:
                raise AssertionError(
                    "isolated SSH client config was not loaded:\n"
                    f"HOME={env['HOME']}\nstdout:\n{ssh_config.stdout}\nstderr:\n{ssh_config.stderr}"
                )
        try:
            hello = json.loads(
                run(
                    [str(binary), "_hello"],
                    cwd=main_clone,
                    env=dict(env, HOME=str(worker_home), WT_CONFIG=str(worker_config)),
                )
            )
            assert hello["role"] == "worker", hello
            assert hello["protocol"] == 3, hello
            snapshot = json.loads(
                run(
                    [str(binary), "_snapshot"],
                    cwd=main_clone,
                    env=dict(env, HOME=str(worker_home), WT_CONFIG=str(worker_config)),
                )
            )
            assert snapshot["protocol"] == 3, snapshot
            assert [row["slug"] for row in snapshot["worktrees"]] == ["remote-check"], snapshot

            version_argv = [str(binary), "remote", "version"]
            first_version = run(version_argv, cwd=main_clone, env=env)
            assert first_version.strip(), "remote version command returned no output"
            second_version = run(version_argv, cwd=main_clone, env=env)
            assert second_version == first_version, (first_version, second_version)
            target = native_target()
            runtimes = list((worker_home / ".cache/wt/native-runtimes" / target).glob("*/wt"))
            assert len(runtimes) == 1, runtimes
            runtime = runtimes[0]
            runtime_hash = hashlib.sha256(binary.read_bytes()).hexdigest()
            assert runtime.parent.name == runtime_hash, runtime
            assert runtime.read_bytes() == binary.read_bytes(), runtime
            runtime.write_bytes(b"corrupted native runtime fixture")
            third_version = run(version_argv, cwd=main_clone, env=env)
            assert third_version == first_version, (first_version, third_version)

            calls = [json.loads(line) for line in ssh_log.read_text().splitlines()]
            remote_calls = [json.loads(line) for line in actual_command_log.read_text().splitlines()]
            assert len(calls) == len(remote_calls), (calls, remote_calls)
            for call, record in zip(calls, remote_calls, strict=True):
                assert call[-2] == "fixture-host", call
                command = record["command"]
                assert record["argv"] == shlex.split(command), record
                assert call[:-2] == [
                    "-o",
                    "BatchMode=yes",
                    "-o",
                    "ConnectTimeout=5",
                    "-o",
                    "ServerAliveInterval=5",
                    "-o",
                    "ServerAliveCountMax=3",
                ], call
            kinds = [record["kind"] for record in remote_calls]
            assert kinds.count("platform") == 6, remote_calls
            assert kinds.count("cache-probe") == 3, remote_calls
            assert kinds.count("upload") == 2, remote_calls
            assert kinds.count("publish") == 2, remote_calls
            encoded_calls = [record["decoded"] for record in remote_calls if record["kind"] == "remote"]
            assert encoded_calls == [["_hello"], ["_hello"], ["version"]] * 3, remote_calls

            runtimes = list((worker_home / ".cache/wt/native-runtimes" / target).glob("*/wt"))
            assert len(runtimes) == 1, runtimes
            runtime = runtimes[0]
            assert runtime.stat().st_mode & 0o111, runtime
            assert runtime.read_bytes() == binary.read_bytes(), runtime
            assert runtime.parent.name == runtime_hash
            assert f'wt_path = {json.dumps(str(binary))}' in controller_config.read_text()
            print(
                json.dumps(
                    {
                        "worker_hello_protocol": hello["protocol"],
                        "snapshot_worktrees": len(snapshot["worktrees"]),
                        "remote_encoded_argv": encoded_calls,
                        "runtime_path": str(runtime),
                        "runtime_sha256": runtime.parent.name,
                        "ssh_transport": transport,
                        "ssh_fallback_reason": fallback_reason,
                    }
                )
            )
        finally:
            if server is not None:
                os.killpg(server.pid, signal.SIGTERM)
                server.wait(timeout=5)


if __name__ == "__main__":
    main()
