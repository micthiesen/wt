#!/usr/bin/env python3
"""Exercise real wt/OpenCode CLI, session discovery, TUI handoffs and scoped removal.

Requires installed OpenCode 2.0.29. All model traffic goes to a loopback fixture;
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
import socket as sockets
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
    native = load("opencode_native", repo / "crates/wt-harness/src/pi/native_smoke.py")
    root, binary = args.output.resolve(), args.binary.resolve()
    main_clone, home, config = fixture.fixture(root, 2)
    # Cargo may publish another debug build during parallel QA. Run an immutable
    # fixture copy so live executable mappings cannot change underneath macOS.
    candidate = root / "fixture-bin/wt"
    shutil.copy2(binary, candidate)
    binary = candidate
    config.write_text(config.read_text() + '\n[harness]\nprimary = "opencode"\nhidden = ["claude", "codex", "pi"]\n')
    # Removal is deliberate only for this isolated unpublished fixture branch.
    (root / "worktrees/bench-000/dirty.txt").unlink()
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), native.Model)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    oc_config = home / ".config/opencode"
    oc_config.mkdir(parents=True)
    skill = home / ".agents/skills/start"
    skill.mkdir(parents=True)
    (skill / "SKILL.md").write_text("---\nname: start\ndescription: Isolated startup proof\n---\nWT_OC_START_SKILL_PROOF: reply without tools.\n")
    (oc_config / "opencode.json").write_text(json.dumps({
        "model": "fixture/fixture", "providers": {"fixture": {
            "name": "Fixture", "env": ["WT_FIXTURE_API_KEY"],
            "package": "@opencode/ai/providers/openai-compatible",
            "settings": {"baseURL": f"http://127.0.0.1:{server.server_port}/v1"},
            "models": {"fixture": {"limit": {"context": 200000, "output": 4096}}},
        }},
    }))
    socket = f"wt-native-opencode-{os.getpid()}"
    env = {"PATH": str(root / "fixture-bin") + os.pathsep + os.environ["PATH"],
           "HOME": str(home), "SHELL": "/bin/sh", "TERM": "xterm-256color", "LANG": "en_US.UTF-8",
           "WT_CONFIG": str(config), "WT_TMUX_SOCKET": socket, "WT_UPDATE": "off", "WT_SKILLS": "off",
           "WT_GITHUB": "off", "WT_AUTOMATIONS": "off", "WT_FETCH_ORIGIN": "off",
           "WT_FIXTURE_API_KEY": "loopback-only", "GIT_CONFIG_NOSYSTEM": "1"}
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
        value = tmux("show-option", "-qv", "-t", f"={slug}-opencode:", "@wt-harness-session-id").stdout.strip()
        assert value, f"missing persisted OpenCode identity for {slug}"
        return value

    def entries(session):
        import sqlite3
        db = home / ".local/share/opencode/opencode.db"
        if not db.exists():
            return []
        with sqlite3.connect(f"file:{db}?mode=ro", uri=True) as conn:
            return [{"type": kind, "data": json.loads(data)} for kind, data in conn.execute(
                "SELECT type,data FROM session_message WHERE session_id=? ORDER BY seq", (session,))]

    def answered(session):
        return any(row["type"] == "assistant" for row in entries(session))

    try:
        assert run(["opencode", "--version"]).stdout.strip() == "opencode v2.0.29", "This test requires OpenCode 2.0.29"
        with sockets.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            port = reservation.getsockname()[1]
        run(["opencode", "service", "set", "port", str(port)])
        # Cold native Agent/Skill readiness reads each have a 45s backend
        # deadline; allow those plus session creation/admission here. Transport
        # errors still fail immediately, and every later command keeps 30s.
        wt("agent", "send", "bench-000", "WT_OC_FIRST_SEND: reply without calling tools.", timeout=180)
        first = identity("bench-000")
        wait(lambda: answered(first), "first real OpenCode answer")
        wt("agent", "start", "bench-001")
        second = identity("bench-001")
        assert first != second, "two worktrees received the same conversation"
        wait(lambda: answered(second), "other session answer")
        assert "WT_OC_START_SKILL_PROOF" in json.dumps(native.Model.requests), "native skill was sent as inert text"
        generated = json.loads(run(["opencode", "api", "experimental.generate.text", "--data",
                                    json.dumps({"prompt": "Reply with a fixture response."})]).stdout)
        assert "Fixture response" in generated["data"]["text"], generated
        wt("agent", "send", "main", "WT_OC_MAIN_ONLY: reply without tools.")
        main_session = identity("main")
        wait(lambda: answered(main_session), "main session answer")
        routes = json.loads(wt("agent", "ls", "--json").stdout)
        for slug in ("bench-000", "bench-001"):
            route = next(row for row in routes if row["target"] == slug)
            assert route["selected_harness"] == "opencode" and route["selection"] == "live", route
            assert "opencode" in route["active_harnesses"], route
        wt("agent", "send", "bench-000", "WT_OC_SECOND_SEND: another response without tools.")
        wait(lambda: sum(row["type"] == "assistant"
                         for row in entries(first)) >= 2, "second answer before compaction")
        wt("agent", "send", "bench-000", "/compact", timeout=140)
        wait(lambda: any(row["type"] == "compaction" for row in entries(first)), "native compaction record", timeout=60)

        child, terminal = pty.fork()
        if child == 0:
            os.chdir(main_clone)
            os.execve(binary, [str(binary)], env)
        fcntl.ioctl(terminal, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 160, 0, 0))
        os.set_blocking(terminal, False)
        wait(lambda: b"Fixture work" in screen.text(), "wt board")
        os.write(terminal, b"g\x1b[24~")
        wait(lambda: tmux("list-clients", "-F", "#{session_name}", check=False).stdout.strip() == "bench-000-opencode", "F12 OpenCode attach")
        tmux("detach-client", "-s", "bench-000-opencode")
        wait(lambda: b"Returned from session" in screen.text(), "return from F12")
        os.write(terminal, b";")
        wait(lambda: b"sessions" in screen.text().lower() and b"OpenCode /" in screen.text()
             and b"waiting" in screen.text(), "OpenCode session picker")
        os.write(terminal, b"\r")
        wait(lambda: tmux("list-clients", "-F", "#{session_name}", check=False).stdout.strip() == "bench-000-opencode", "picker attaches exact live OpenCode")
        assert identity("bench-000") == first, "picker replaced the existing OpenCode conversation"
        tmux("detach-client", "-s", "bench-000-opencode")
        wait(lambda: b"Returned from session" in screen.text(), "return from picker")
        tmux("kill-session", "-t", "=bench-000-opencode")
        os.write(terminal, b"\x1b[24~")
        wait(lambda: tmux("list-clients", "-F", "#{session_name}", check=False).stdout.strip() == "bench-000-opencode", "resume closed OpenCode slot")
        assert identity("bench-000") == first, "F12 did not resume the saved conversation"
        tmux("detach-client", "-s", "bench-000-opencode")
        wait(lambda: b"Returned from session" in screen.text(), "return from resumed session")
        os.write(terminal, b"m")
        wait(lambda: tmux("list-clients", "-F", "#{session_name}", check=False).stdout.strip() == "manager-opencode", "manager entry")
        assert identity("manager") != main_session, "manager borrowed main's live conversation"
        tmux("detach-client", "-s", "manager-opencode")
        wait(lambda: b"Returned from session" in screen.text(), "return from manager")
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
        assert tmux("has-session", "-t", "=bench-000-opencode", check=False).returncode != 0
        assert identity("bench-001") == second, "removal disturbed the other OpenCode session"
        report = {"passed": True, "model_calls": native.Model.calls, "first_session": first,
                  "other_session": second, "checks": ["send", "native startup skill", "stateless generation", "live discovery", "native compaction",
                  "F12 attach/return", "session picker identity", "resume", "manager/main ownership", "scoped removal"]}
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
        run(["opencode", "service", "stop"], check=False)
        server.shutdown()
        server.server_close()


if __name__ == "__main__":
    main()
