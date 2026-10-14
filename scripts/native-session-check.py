#!/usr/bin/env python3
"""Verify TUI shell handoff and the live agent-session picker in private tmux."""
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

# The list marks the cursor row with the selection background, not a glyph.
SELECTED_ROW = b"48;2;59;66;82m"


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
    # Fail closed if the picker accidentally starts a new agent instead of
    # attaching the witnessed slot. Never launch the user's actual Codex.
    unexpected_launch = root / "unexpected-codex-launch"
    codex_stub = root / "fixture-bin" / "codex"
    codex_stub.write_text(f"#!/bin/sh\nprintf unexpected > {shlex.quote(str(unexpected_launch))}\nexit 97\n")
    codex_stub.chmod(0o700)
    env = dict(os.environ, HOME=str(home), WT_CONFIG=str(config), TERM="xterm-256color",
               CODEX_HOME=str(home / ".codex"),
               PATH=str(root / "fixture-bin") + os.pathsep + os.environ["PATH"],
               WT_TMUX_SOCKET=socket, SHELL="/bin/sh", WT_UPDATE="off", WT_SKILLS="off",
               WT_GITHUB="off", WT_AUTOMATIONS="off")
    for key in ("WT_REPO_CONFIG", "WT_REPO_ID", "WT_AGENT", "TMUX", "TMUX_PANE",
                "XDG_CONFIG_HOME", "XDG_CACHE_HOME", "XDG_STATE_HOME", "NO_COLOR", "NO_COLOUR"):
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
                return bytes(output)
        raise AssertionError(f"terminal condition timed out; inspect {root / 'terminal.ansi'}")

    def tmux(*args):
        return subprocess.run(["tmux", "-L", socket, *args], env=env, cwd=home,
                              capture_output=True, text=True, timeout=5)

    def session_id(name):
        sessions = tmux("list-sessions", "-F", "#{session_name}|#{session_id}")
        assert sessions.returncode == 0, sessions.stderr
        matches = [line.split("|", 1)[1] for line in sessions.stdout.splitlines()
                   if line.startswith(name + "|")]
        assert len(matches) == 1 and matches[0].startswith("$"), (name, sessions.stdout)
        return matches[0]

    def printed(data):
        return re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", data).replace(b" ", b"")

    try:
        wait_for(lambda data: b"2 worktrees" in data)
        os.write(terminal, b"\x1b[21~")  # F10: selected worktree shell.
        wait_for(lambda _: tmux("list-clients", "-F", "#{session_name}").stdout.strip() == "bench-000-shell")
        marker = root / "shell-input-received"
        os.write(terminal, f"printf ok > {shlex.quote(str(marker))}\r".encode())
        # Shell redirection creates the file before printf writes its contents.
        # Readiness is the complete acknowledgement, not directory visibility.
        wait_for(lambda _: marker.exists() and marker.read_text() == "ok")
        assert marker.read_text() == "ok", "attached shell did not receive intact input"
        detached = tmux("detach-client", "-s", "bench-000-shell")
        assert detached.returncode == 0, detached.stderr
        # Ratatui may advance over unchanged blank cells with cursor commands.
        wait_for(lambda data: b"Returnedfromsession" in printed(data))
        # Model the startup window before Codex publishes a conversation UUID.
        # These are plain shells in this test's private server, never agents.
        for name in ("bench-000-codex", "picker-unrelated"):
            created = tmux("new-session", "-d", "-s", name, "-c",
                           str(root / "worktrees" / "bench-000"), "/bin/sh")
            assert created.returncode == 0, created.stderr
        unknown_id = session_id("bench-000-codex")
        unrelated_id = session_id("picker-unrelated")
        assert not tmux("show-option", "-qv", "-t", "=bench-000-codex", "@wt-harness-session-id").stdout.strip()
        os.write(terminal, b";")
        wait_for(lambda data: b"currentsession(conversationnotidentified)" in printed(data)
                 and b"live" in printed(data))
        os.write(terminal, b"\r")
        wait_for(lambda _: tmux("list-clients", "-F", "#{session_id}").stdout.strip() == unknown_id)
        picker_marker = root / "picker-input-received"
        os.write(terminal, f"printf picker-ok > {shlex.quote(str(picker_marker))}\r".encode())
        wait_for(lambda _: picker_marker.exists() and picker_marker.read_text() == "picker-ok")
        assert picker_marker.read_text() == "picker-ok", "picker did not attach the live shell slot"
        assert not unexpected_launch.exists(), "picker started Codex instead of attaching the existing slot"
        detached = tmux("detach-client", "-s", "bench-000-codex")
        assert detached.returncode == 0, detached.stderr
        wait_for(lambda data: b"Returnedfromsession" in printed(data))
        os.write(terminal, b";")
        wait_for(lambda data: b"currentsession(conversationnotidentified)" in printed(data)
                 and b"live" in printed(data))
        os.write(terminal, b"x")  # Kill only the picker row's witnessed live slot.
        wait_for(lambda _: tmux("has-session", "-t", "=bench-000-codex").returncode != 0)
        assert session_id("picker-unrelated") == unrelated_id
        assert tmux("has-session", "-t", "=bench-000-shell").returncode == 0
        # Graceful close must remain pinned to the original pane. When EOF
        # closes it, tmux activates the second pane; another EOF must not leak
        # into that unrelated shell after the 200 ms close retry delay.
        created = tmux("new-session", "-d", "-P", "-F", "#{pane_id}|#{pane_pid}",
                       "-s", "bench-000-codex", "-c",
                       str(root / "worktrees" / "bench-000"), "/bin/sh")
        assert created.returncode == 0, created.stderr
        first_pane, first_pid = created.stdout.strip().split("|")
        split = tmux("split-window", "-d", "-P", "-F", "#{pane_id}|#{pane_pid}",
                     "-t", first_pane, "/bin/sh")
        assert split.returncode == 0, split.stderr
        other_pane, other_pid = split.stdout.strip().split("|")
        assert first_pane.startswith("%") and other_pane.startswith("%")
        assert first_pane != other_pane and first_pid != other_pid
        selected = tmux("select-pane", "-t", first_pane)
        assert selected.returncode == 0, selected.stderr
        graceful_session = session_id("bench-000-codex")
        os.write(terminal, b";")
        wait_for(lambda data: b"currentsession(conversationnotidentified)" in printed(data)
                 and b"live" in printed(data))
        close_started = time.monotonic()
        os.write(terminal, b"d")
        # The picker opens from the board, whose tmux inventory may still name
        # the slot killed above. Its stale witness must be refused without
        # touching either pane; the picker reopens and refreshes, and the
        # repeated key then closes the exact current slot.
        for _ in range(5):
            try:
                wait_for(lambda data: b"ClosedCodexsession" in printed(data), timeout=3)
                break
            except AssertionError:
                # Refused as stale: nothing may have closed. A failure toast
                # stays on screen, so retry on time, never on its text.
                assert b"Changedsinceyoulooked" in printed(bytes(transcript))
                assert f"{first_pane}|{first_pid}" in tmux(
                    "list-panes", "-s", "-t", graceful_session, "-F", "#{pane_id}|#{pane_pid}"
                ).stdout.splitlines(), "a stale picker closed a pane"
                os.write(terminal, b"d")
        else:
            raise AssertionError("the reopened picker never closed the current slot")
        wait_for(lambda _: time.monotonic() - close_started >= 0.3)
        panes = tmux("list-panes", "-s", "-t", graceful_session,
                     "-F", "#{pane_id}|#{pane_pid}")
        assert panes.returncode == 0, "graceful close killed the other pane's session"
        assert f"{first_pane}|{first_pid}" not in panes.stdout.splitlines(), "selected pane did not close"
        assert f"{other_pane}|{other_pid}" in panes.stdout.splitlines(), "EOF leaked into the replacement active pane"
        os.kill(int(other_pid), 0)  # The exact other shell process must still exist.
        assert session_id("picker-unrelated") == unrelated_id
        assert tmux("has-session", "-t", "=bench-000-shell").returncode == 0
        os.write(terminal, b"j")
        wait_for(lambda data: SELECTED_ROW in data)
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
                  "shell_survived_ui_exit": True, "clean_shutdown": True,
                  "unidentified_live_picker_attached": True, "picker_received_input": True,
                  "picker_killed_exact_slot": True, "unrelated_session_preserved": True,
                  "graceful_close_preserved_other_pane": True,
                  "no_agent_launched": not unexpected_launch.exists()}
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
