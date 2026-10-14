#!/usr/bin/env python3
"""Exercise real wt/Pi CLI, session discovery, TUI handoffs and scoped removal.

Requires installed Pi 1.2.0. All model traffic goes to a loopback fixture;
user provider credentials, sessions and tmux servers are never used.
"""
import argparse
import errno
import fcntl
import http.server
import importlib.util
import json
import os
from pathlib import Path
import pty
import select
import shutil
import signal
import struct
import subprocess
import termios
import threading
import time


def load(name, path):
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    args = parser.parse_args()
    repo = Path(__file__).resolve().parent.parent
    fixture = load("wt_perf", repo / "scripts/perf-baseline.py")
    ui = load("wt_ui", repo / "scripts/native-ui-check.py")
    native = load("pi_native", repo / "crates/wt-harness/src/pi/native_smoke.py")
    root, binary = args.output.resolve(), args.binary.resolve()
    main_clone, home, config = fixture.fixture(root, 2)
    # Cargo may publish another debug build during parallel QA. Run an immutable
    # fixture copy so live executable mappings cannot change underneath macOS.
    candidate = root / "fixture-bin/wt"
    shutil.copy2(binary, candidate)
    binary = candidate
    config.write_text(config.read_text() + '\n[harness]\nprimary = "pi"\nhidden = ["claude", "codex", "opencode"]\n')
    # Removal is deliberate only for this isolated unpublished fixture branch.
    (root / "worktrees/bench-000/dirty.txt").unlink()
    agent = home / ".pi/agent"
    agent.mkdir(parents=True)
    skills = agent / "skills/start"
    skills.mkdir(parents=True)
    skills.joinpath("SKILL.md").write_text(
        "---\nname: start\ndescription: Isolated wt Pi startup proof\n---\n"
        "WT_PI_START_SKILL_PROOF: reply with a fixture response; do not call tools.\n"
    )
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), native.Model)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    (agent / "settings.json").write_text(json.dumps({
        "defaultProvider": "fixture", "defaultModel": "fixture", "defaultThinkingLevel": "off",
        "defaultProjectTrust": "always", "packages": [], "enableInstallTelemetry": False,
        "compaction": {"enabled": False, "keepRecentTokens": 100, "reserveTokens": 1024},
    }))
    (agent / "models.json").write_text(json.dumps({"providers": {"fixture": {
        "baseUrl": f"http://127.0.0.1:{server.server_port}/v1", "api": "openai-completions",
        "apiKey": "loopback-fixture-only", "models": [{"id": "fixture", "name": "Fixture",
        "reasoning": False, "contextWindow": 200000, "maxTokens": 4096}],
    }}}))
    socket = f"wt-native-pi-{os.getpid()}"
    # Start from a whitelist, so no inherited provider key or agent config can
    # redirect either Pi or wt into the user's real accounts and sessions.
    env = {"PATH": str(root / "fixture-bin") + os.pathsep + os.environ["PATH"],
           "HOME": str(home), "SHELL": "/bin/sh", "TERM": "xterm-256color", "LANG": "en_US.UTF-8",
           "WT_CONFIG": str(config), "WT_TMUX_SOCKET": socket, "WT_UPDATE": "off", "WT_SKILLS": "off",
           "WT_GITHUB": "off", "WT_AUTOMATIONS": "off", "WT_FETCH_ORIGIN": "off",
           "PI_CODING_AGENT_DIR": str(agent), "PI_CODING_AGENT_SESSION_DIR": str(root / "sessions"),
           "PI_OFFLINE": "1", "PI_TELEMETRY": "0", "GIT_CONFIG_NOSYSTEM": "1"}
    transcript, screen = bytearray(), ui.AnsiScreen()
    child, terminal, reaped = None, None, False

    def run(argv, timeout=30, check=True):
        result = subprocess.run(argv, cwd=main_clone, env=env, text=True, capture_output=True, timeout=timeout)
        with (root / "commands.log").open("a") as output:
            output.write(f"$ {argv!r}\n{result.stdout}{result.stderr}\n")
        if check:
            assert result.returncode == 0, (argv, result.stdout, result.stderr)
        return result

    def wt(*argv, **kwargs):
        return run([str(binary), *argv], **kwargs)

    def tmux(*argv, **kwargs):
        return run(["tmux", "-L", socket, *argv], **kwargs)

    def drain():
        if terminal is None:
            return
        while select.select([terminal], [], [], 0)[0]:
            try:
                data = os.read(terminal, 65536)
            except OSError as error:
                if error.errno == errno.EIO:
                    return
                raise
            if not data:
                return
            transcript.extend(data)
            screen.feed(data)

    def wait(predicate, label, timeout=20):
        deadline = time.monotonic() + timeout
        while time.monotonic() < deadline:
            drain()
            if predicate():
                return
            time.sleep(.05)
        raise AssertionError(f"Timed out: {label}; see {root}")

    def identity(slug):
        value = tmux("show-option", "-qv", "-t", f"={slug}-pi:", "@wt-harness-session-id").stdout.strip()
        assert value, f"missing persisted Pi identity for {slug}"
        return value

    def entries(session):
        files = list((root / "sessions").rglob(f"*{session}*.jsonl"))
        if len(files) != 1:
            return []
        return [json.loads(line) for line in files[0].read_text().splitlines() if line.strip()]

    def answered(session):
        return any(row.get("type") == "message" and row.get("message", {}).get("role") == "assistant"
                   for row in entries(session))

    try:
        assert run(["pi", "--version"]).stdout.strip() == "1.2.0", "This test requires Pi 1.2.0"
        wt("agent", "send", "bench-000", "WT_PI_FIRST_SEND: reply without calling tools.")
        first = identity("bench-000")
        wait(lambda: answered(first), "first real Pi answer")
        wt("agent", "start", "bench-001")
        second = identity("bench-001")
        assert first != second, "two worktrees received the same conversation"
        wait(lambda: answered(second), "native start-skill answer")
        for slug, session in (("bench-000", first), ("bench-001", second)):
            header = next(row for row in entries(session) if row.get("type") == "session")
            assert header["id"] == session and Path(header["cwd"]).resolve() == root / "worktrees" / slug, header
        assert "WT_PI_START_SKILL_PROOF" in json.dumps(native.Model.requests), "start was sent as inert text"
        routes = json.loads(wt("agent", "ls", "--json").stdout)
        for slug in ("bench-000", "bench-001"):
            route = next(row for row in routes if row["target"] == slug)
            assert route["selected_harness"] == "pi" and route["selection"] == "live", route
            assert "pi" in route["active_harnesses"], route
        wt("agent", "send", "bench-000", "WT_PI_SECOND_SEND: another response without tools.")
        wait(lambda: sum(row.get("type") == "message" and row.get("message", {}).get("role") == "assistant"
                         for row in entries(first)) >= 2, "second answer before compaction")
        wt("agent", "send", "bench-000", "/compact", timeout=140)
        assert any(row.get("type") == "compaction" for row in entries(first)), "no native compaction record"

        child, terminal = pty.fork()
        if child == 0:
            os.chdir(main_clone)
            os.execve(binary, [str(binary)], env)
        fcntl.ioctl(terminal, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 160, 0, 0))
        os.set_blocking(terminal, False)
        wait(lambda: b"Fixture work" in screen.text(), "wt board")
        os.write(terminal, b"g\x1b[24~")
        wait(lambda: tmux("list-clients", "-F", "#{session_name}", check=False).stdout.strip() == "bench-000-pi", "F12 Pi attach")
        tmux("detach-client", "-s", "bench-000-pi")
        wait(lambda: b"Returned from session" in screen.text(), "return from F12")
        os.write(terminal, b";")
        wait(lambda: b"sessions" in screen.text().lower() and first[:8].encode() in screen.text()
             and b"waiting" in screen.text(), "Pi session picker")
        os.write(terminal, b"\r")
        wait(lambda: tmux("list-clients", "-F", "#{session_name}", check=False).stdout.strip() == "bench-000-pi", "picker attaches exact live Pi")
        assert identity("bench-000") == first, "picker replaced the existing Pi conversation"
        tmux("detach-client", "-s", "bench-000-pi")
        wait(lambda: b"Returned from session" in screen.text(), "return from picker")
        os.write(terminal, b"q")
        deadline = time.monotonic() + 10
        while time.monotonic() < deadline:
            drain()
            ended, status = os.waitpid(child, os.WNOHANG)
            if ended:
                reaped = True
                assert os.waitstatus_to_exitcode(status) == 0
                break
            time.sleep(.05)
        assert reaped, "wt did not exit promptly"
        wt("rm", "bench-000", "--yes", "--force", "--keep-branch", "--no-destroy-stage", timeout=60)
        assert not (root / "worktrees/bench-000").exists(), "worktree removal did not complete"
        assert tmux("has-session", "-t", "=bench-000-pi", check=False).returncode != 0
        assert identity("bench-001") == second, "removal disturbed the other Pi session"
        report = {"passed": True, "model_calls": native.Model.calls, "first_session": first,
                  "other_session": second, "checks": ["send", "start skill", "live discovery", "native compaction",
                  "F12 attach/return", "session picker identity", "scoped removal"]}
        (root / "result.json").write_text(json.dumps(report, indent=2) + "\n")
        print(json.dumps(report))
    finally:
        (root / "terminal.ansi").write_bytes(transcript)
        (root / "screen.txt").write_bytes(screen.text())
        if child and not reaped:
            try:
                os.kill(child, signal.SIGTERM)
                deadline = time.monotonic() + 5
                while time.monotonic() < deadline:
                    if os.waitpid(child, os.WNOHANG)[0]:
                        break
                    time.sleep(.05)
                else:
                    os.kill(child, signal.SIGKILL)
                    deadline = time.monotonic() + 2
                    while time.monotonic() < deadline:
                        if os.waitpid(child, os.WNOHANG)[0]:
                            break
                        time.sleep(.05)
            except ProcessLookupError:
                pass
        if terminal is not None:
            drain()
            os.close(terminal)
        (root / "terminal.ansi").write_bytes(transcript)
        tmux("kill-server", check=False)
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
