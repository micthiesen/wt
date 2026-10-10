#!/usr/bin/env python3
"""Verify observed terminal colors reach Codex through wt's production tmux config."""

from __future__ import annotations

import os
import pathlib
import re
import shutil
import json
import errno
import fcntl
import importlib.util
import pty
import select
import signal
import struct
import subprocess
import termios
import tempfile
import time


def require(name: str) -> str:
    value = shutil.which(name)
    if not value:
        raise RuntimeError(f"{name} must be installed and available on PATH")
    return value


def run(args: list[str], env: dict[str, str], timeout: int = 20) -> str:
    result = subprocess.run(args, env=env, text=True, capture_output=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError(f"{args[0]} {args[1:3]} failed: {result.stderr.strip()}")
    return result.stdout


def probe(
    label: str,
    *,
    configured: bool,
    scratch: pathlib.Path,
    generated_config: pathlib.Path | None = None,
) -> None:
    home = scratch / label
    home.mkdir()
    cache = home / "cache"
    cache.mkdir()
    socket = home / "tmux.sock"
    config = home / ".tmux.conf"
    config.write_text("set -g status off\nset -g default-terminal tmux-256color\n")
    env = {
        "PATH": os.environ["PATH"],
        "HOME": str(home),
        "CODEX_HOME": str(home),
        "TERM": "xterm-256color",
        "TMPDIR": str(scratch),
    }

    def tmux(*args: str) -> str:
        return run(["tmux", "-S", str(socket), "-f", str(config), *args], env)

    try:
        if configured:
            if generated_config is None:
                raise RuntimeError("configured Codex probe requires a native-observed palette config")
            config = generated_config

        cwd = home / "project"
        cwd.mkdir()
        run([
            "tmux", "-S", str(socket), "-f", str(config), "new-session", "-d", "-s", "probe",
            "-x", "120", "-y", "40", "codex", "-C", str(cwd), "--no-alt-screen",
            "-c", 'model_provider="probe"', "-c", 'model="probe"', "-c",
            'model_providers.probe.name="probe"', "-c",
            'model_providers.probe.base_url="http://127.0.0.1:1/v1"', "-c",
            'model_providers.probe.wire_api="responses"',
        ], env)
        deadline = time.monotonic() + 30
        screen = ""
        while time.monotonic() < deadline:
            screen = tmux("capture-pane", "-p", "-e", "-t", "=probe:")
            if "Trust and continue" in screen:
                tmux("send-keys", "-t", "=probe:", "Enter")
            if "probe default" in screen:
                break
            time.sleep(0.1)
        if "probe default" not in screen:
            raise RuntimeError(f"{label}: Codex composer did not initialize:\n{screen}")
        composer = next((line for line in screen.splitlines() if "Ask Codex" in line), None)
        if composer is None:
            raise RuntimeError(f"{label}: composer line not found:\n{screen}")
        backgrounds = re.findall(r"\x1b\[([^m]*?48;2;\d+;\d+;\d+[^m]*)m", composer)
        print(f"{label}: composer background attributes {backgrounds}")
        if configured and not backgrounds:
            raise RuntimeError("production palette config did not shade the Codex composer")
        if configured and all("48;2;30;30;46" in value for value in backgrounds):
            raise RuntimeError("composer used only the terminal default background")
        if not configured and backgrounds:
            raise RuntimeError("baseline unexpectedly has explicit RGB composer shading")
    finally:
        subprocess.run(["tmux", "-S", str(socket), "kill-server"], env=env, timeout=5, capture_output=True)


def observe_from_native_tui(binary: pathlib.Path, scratch: pathlib.Path) -> pathlib.Path:
    fixture_path = pathlib.Path(__file__).with_name("perf-baseline.py")
    spec = importlib.util.spec_from_file_location("wt_palette_fixture", fixture_path)
    if spec is None or spec.loader is None:
        raise RuntimeError("could not load the isolated Git fixture helper")
    fixture = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(fixture)

    fixture_root = scratch / "tui-observation"
    main_clone, home, config = fixture.fixture(fixture_root, 2)
    socket = f"wt-palette-smoke-{os.getpid()}"
    env = dict(
        os.environ,
        HOME=str(home),
        WT_CONFIG=str(config),
        WT_TMUX_SOCKET=socket,
        TERM="xterm-256color",
        COLORTERM="truecolor",
        WT_UPDATE="off",
        WT_SKILLS="off",
        WT_GITHUB="off",
        WT_AUTOMATIONS="off",
        SHELL="/bin/sh",
    )
    for key in (
        "WT_REPO_CONFIG",
        "WT_REPO_ID",
        "WT_AGENT",
        "TMUX",
        "TMUX_PANE",
        "XDG_CONFIG_HOME",
        "XDG_CACHE_HOME",
        "XDG_STATE_HOME",
    ):
        env.pop(key, None)

    pid, terminal = pty.fork()
    if pid == 0:
        os.chdir(main_clone)
        os.execve(binary, [str(binary)], env)
    fcntl.ioctl(terminal, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 160, 0, 0))
    os.set_blocking(terminal, False)
    transcript = bytearray()
    reaped = False
    responded = False

    def read(timeout: float = 0.02) -> bytes:
        if not select.select([terminal], [], [], timeout)[0]:
            return b""
        try:
            chunk = os.read(terminal, 65536)
        except OSError as error:
            if error.errno == errno.EIO:
                return b""
            raise
        transcript.extend(chunk)
        return chunk

    def visible(data: bytes) -> bytes:
        return re.sub(rb"\x1b\[[0-?]*[ -/]*[@-~]", b"", data)

    def wait_for(predicate, timeout: float = 15) -> None:
        nonlocal responded
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            read()
            if (
                not responded
                and b"\x1b]10;?\x1b\\" in transcript
                and b"\x1b]11;?\x1b\\" in transcript
            ):
                # Fragment both OSC replies and put a help key between them.
                # The native startup scanner must replay that key.
                for part in (
                    b"\x1b]10;rgb:cdcd",
                    b"/d6d6/f4f4\x1b\\?\x1b]11;rgb:1e1e",
                    b"/1e1e/2e2e\x07",
                ):
                    os.write(terminal, part)
                responded = True
            if predicate(bytes(transcript)):
                return
        raise AssertionError(f"native palette startup timed out; captured {len(transcript)} terminal bytes")

    cache = fixture_root / "cache"
    palette_path = cache / "terminal-palette.json"
    generated_config = cache / "tmux-palette.conf"
    try:
        wait_for(lambda _: palette_path.exists() and generated_config.exists())
        palette = json.loads(palette_path.read_text())
        if palette != {"defaultForeground": "#cdd6f4", "defaultBackground": "#1e1e2e"}:
            raise AssertionError(f"native startup persisted unexpected colors: {palette}")
        rendered = generated_config.read_text()
        if "window-active-style 'fg=#cdd6f4,bg=#1e1e2e'" not in rendered:
            raise AssertionError(f"native startup omitted the observed tmux palette: {rendered!r}")

        # The interleaved `?` must open and close help before the TUI exits.
        wait_for(lambda data: b"wt keymap" in visible(data))
        os.write(terminal, b"\x1b")
        deadline = time.monotonic() + 0.2
        while time.monotonic() < deadline:
            read(0.01)
        os.write(terminal, b"q")
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            waited, _ = os.waitpid(pid, os.WNOHANG)
            if waited:
                reaped = True
                break
            read()
            time.sleep(0.02)
        if not reaped:
            raise AssertionError("native TUI did not exit after quit")
        return generated_config
    finally:
        if not reaped:
            os.kill(pid, signal.SIGTERM)
            try:
                os.waitpid(pid, 0)
            except ChildProcessError:
                pass
        subprocess.run(["tmux", "-L", socket, "kill-server"], env=env, timeout=5, capture_output=True)


def main() -> None:
    import argparse

    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=pathlib.Path, required=True, help="built native wt executable")
    args = parser.parse_args()
    require("codex")
    require("tmux")
    scratch = pathlib.Path(tempfile.mkdtemp(prefix="wtcp-", dir="/tmp"))
    try:
        print(run(["codex", "--version"], {"PATH": os.environ["PATH"]}).strip())
        generated_config = observe_from_native_tui(args.binary.resolve(), scratch)
        print(f"native TUI observed and persisted its terminal palette to {generated_config}")
        probe("baseline", configured=False, scratch=scratch)
        probe("observed-palette", configured=True, scratch=scratch, generated_config=generated_config)
        print("PASS native OSC observation reaches the private tmux server and actual Codex composer.")
        print("No credentials or model requests were used; Codex state and tmux servers were isolated.")
    finally:
        shutil.rmtree(scratch, ignore_errors=True)


if __name__ == "__main__":
    main()
