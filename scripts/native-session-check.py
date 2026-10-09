#!/usr/bin/env python3
"""Verify real TUI -> private tmux shell -> TUI terminal ownership handoff."""
import argparse
import errno
import fcntl
import importlib.util
import json
import os
from pathlib import Path
import pty
import re
import select
import shlex
import signal
import struct
import subprocess
import termios
import time


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    root = args.output.resolve()
    binary = args.binary.resolve()
    spec = importlib.util.spec_from_file_location("wt_perf", Path(__file__).with_name("perf-baseline.py"))
    fixture = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(fixture)
    main_clone, home, config = fixture.fixture(root, 2)
    socket = f"wt-native-session-{os.getpid()}"
    env = dict(os.environ, HOME=str(home), WT_CONFIG=str(config), TERM="xterm-256color",
               WT_TMUX_SOCKET=socket, SHELL="/bin/sh", WT_UPDATE="off", WT_SKILLS="off",
               WT_GITHUB="off", WT_AUTOMATIONS="off")
    for key in ("WT_REPO_CONFIG", "WT_REPO_ID", "WT_AGENT", "TMUX", "TMUX_PANE",
                "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME"):
        env.pop(key, None)
    pid, terminal = pty.fork()
    if pid == 0:
        os.chdir(main_clone)
        os.execve(binary, [str(binary)], env)
    fcntl.ioctl(terminal, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 160, 0, 0))
    os.set_blocking(terminal, False)
    transcript = bytearray()
    reaped = False

    def wait_for(predicate, timeout=10):
        output = bytearray()
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            if select.select([terminal], [], [], 0.01)[0]:
                try:
                    data = os.read(terminal, 65536)
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
                    data = b""
                transcript.extend(data)
                output.extend(data)
            if predicate(bytes(output)):
                return
        raise AssertionError(f"terminal condition timed out; inspect {root / 'terminal.ansi'}")

    def tmux(*args):
        return subprocess.run(["tmux", "-L", socket, *args], env=env, cwd=home,
                              capture_output=True, text=True, timeout=5)

    def printed(data):
        return re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", data).replace(b" ", b"")

    try:
        wait_for(lambda data: b"2 worktrees" in data)
        os.write(terminal, b"\x1b[21~")  # F10: selected worktree shell.
        wait_for(lambda _: tmux("list-clients", "-F", "#{session_name}").stdout.strip() == "bench-000-shell")
        marker = root / "shell-input-received"
        os.write(terminal, f"printf ok > {shlex.quote(str(marker))}\r".encode())
        wait_for(lambda _: marker.exists())
        assert marker.read_text() == "ok", "attached shell did not receive intact input"
        detached = tmux("detach-client", "-s", "bench-000-shell")
        assert detached.returncode == 0, detached.stderr
        # Ratatui may advance over unchanged blank cells with cursor commands.
        wait_for(lambda data: b"Returnedfromsession" in printed(data))
        os.write(terminal, b"j")
        wait_for(lambda data: "›".encode() in data)
        os.write(terminal, b"t\x15After session\r")
        wait_for(lambda data: b"Titlesaved" in printed(data))
        os.write(terminal, b"q")
        status = None
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            result = os.waitpid(pid, os.WNOHANG)
            if result[0]:
                status = result[1]
                reaped = True
                break
            if select.select([terminal], [], [], 0.01)[0]:
                try:
                    transcript.extend(os.read(terminal, 65536))
                except OSError as error:
                    if error.errno != errno.EIO:
                        raise
        assert status == 0, f"UI did not exit cleanly after session handoff: {status}"
        assert tmux("has-session", "-t", "=bench-000-shell").returncode == 0, "UI quit killed user shell"
        result = {"shell_received_input": True, "ui_resumed": True, "title_saved_after_resume": True,
                  "shell_survived_ui_exit": True, "clean_shutdown": True}
        (root / "result.json").write_text(json.dumps(result) + "\n")
        print(json.dumps(result))
    finally:
        (root / "terminal.ansi").write_bytes(transcript)
        os.close(terminal)
        if not reaped:
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)
        tmux("kill-server")


if __name__ == "__main__":
    main()
