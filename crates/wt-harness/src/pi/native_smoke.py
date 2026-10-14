#!/usr/bin/env python3
"""Exercise the installed native Pi TUI against a loopback-only mock model.

No user sessions, provider credentials, or paid requests are used.
Run: python3 crates/wt-harness/src/pi/native_smoke.py
"""
import http.server
import json
import os
from pathlib import Path
import pty
import select
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time


class Model(http.server.BaseHTTPRequestHandler):
    calls = 0
    requests = []

    def log_message(self, *args):
        pass

    def do_POST(self):
        Model.requests.append(json.loads(self.rfile.read(int(self.headers["Content-Length"]))))
        Model.calls += 1
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.end_headers()
        for chunk in [
            {"choices": [{"index": 0, "delta": {"role": "assistant", "content": "Fixture response. " * 100}, "finish_reason": None}]},
            {"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}], "usage": {"prompt_tokens": 50000, "completion_tokens": 100, "total_tokens": 50100}},
        ]:
            self.wfile.write(("data: " + json.dumps({"id": "fixture", "object": "chat.completion.chunk", "created": 1, "model": "fixture", **chunk}) + "\n\n").encode())
            self.wfile.flush()
        self.wfile.write(b"data: [DONE]\n\n")


def main():
    extension = Path(__file__).with_name("extension.ts").resolve()
    model = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Model)
    threading.Thread(target=model.serve_forever, daemon=True).start()
    with tempfile.TemporaryDirectory(prefix="wt-pi-native-", dir="/tmp") as scratch:
        root = Path(scratch)
        agent = root / "agent"
        agent.mkdir()
        skill = root / "fixture-skill.md"
        skill.write_text("---\nname: wt-fixture\ndescription: Isolated wt integration test\n---\nWT_SKILL_EXPANSION_PROOF: reply with a fixture response.\n")
        (agent / "settings.json").write_text(json.dumps({"compaction": {"enabled": False, "keepRecentTokens": 100, "reserveTokens": 1024}, "enableInstallTelemetry": False}))
        (agent / "models.json").write_text(json.dumps({"providers": {"fixture": {
            "baseUrl": f"http://127.0.0.1:{model.server_port}/v1", "api": "openai-completions", "apiKey": "fixture-only",
            "models": [{"id": "fixture", "name": "Fixture", "reasoning": False, "contextWindow": 200000, "maxTokens": 4096}]
        }}}))
        control = root / "control.sock"
        env = {**os.environ, "PI_CODING_AGENT_DIR": str(agent), "PI_CODING_AGENT_SESSION_DIR": str(root / "sessions"), "PI_TELEMETRY": "0", "TERM": "xterm-256color"}
        tmux = ["tmux", "-S", str(root / "tmux.sock"), "-f", "/dev/null"] if "--tmux" in sys.argv else None
        argv = ["pi", "--offline", "--no-extensions", "--no-skills", "--no-context-files", "--no-mcp", "--extension", str(extension),
            "--wt-control", str(control), "--skill", str(skill), "--session-id", "wt-pi-native-test", "--model", "fixture/fixture", "--approve"]
        if tmux:
            argv += ["--wt-tmux-session", "fixture"]
            subprocess.run([*tmux, "new-session", "-d", "-s", "fixture", "-x", "120", "-y", "40", "-c", str(root), *argv], env=env, check=True)
            child = None
        else:
            master, slave = pty.openpty()
            child = subprocess.Popen(argv, cwd=root, env=env, stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
            os.close(slave)
        output = bytearray()

        def drain():
            if tmux:
                captured = subprocess.run([*tmux, "capture-pane", "-p", "-t", "fixture:0.0"], capture_output=True)
                output[:] = captured.stdout + captured.stderr
                return
            while select.select([master], [], [], 0)[0]:
                try:
                    block = os.read(master, 65536)
                except OSError:
                    break
                if not block:
                    break
                output.extend(block)

        def request(op, id="status", **extra):
            with socket.socket(socket.AF_UNIX) as conn:
                conn.settimeout(4)
                conn.connect(str(control))
                conn.sendall((json.dumps({"id": id, "session_id": "wt-pi-native-test", "cwd": str(root.resolve()), "op": op, **extra}) + "\n").encode())
                data = b""
                while b"\n" not in data:
                    data += conn.recv(65536)
                result = json.loads(data)
                assert result["ok"], result
                return result["result"]

        def wait(predicate, label, timeout=20):
            until = time.monotonic() + timeout
            while time.monotonic() < until:
                drain()
                alive = subprocess.run([*tmux, "has-session", "-t", "=fixture"], capture_output=True).returncode == 0 if tmux else child.poll() is None
                assert alive, output.decode(errors="replace")[-2000:]
                if predicate():
                    return
                time.sleep(.05)
            raise AssertionError(f"Timed out: {label}\n" + output.decode(errors="replace")[-2000:])

        try:
            wait(control.exists, "native extension socket")
            assert request("status")["session_id"] == "wt-pi-native-test"
            if tmux:
                identity = subprocess.check_output([*tmux, "show-options", "-qv", "-t", "=fixture:", "@wt-harness-session-id"], text=True).strip()
                assert identity == "wt-pi-native-test", identity
            assert request("send", "send-1", text="/skill:wt-fixture")["status"] == "accepted"
            wait(lambda: Model.calls >= 1 and request("status")["state"] == "waiting", "first response")
            assert "WT_SKILL_EXPANSION_PROOF" in json.dumps(Model.requests[0]), "skill command was not expanded"
            assert request("send", "send-1", text="/skill:wt-fixture")["status"] == "accepted"
            assert Model.calls == 1, "duplicate request caused a second model turn"
            request("send", "send-2", text="Another fixture response please.")
            wait(lambda: Model.calls >= 2 and request("status")["state"] == "waiting", "second response")
            assert request("compact", "compact-1")["status"] in ("accepted", "completed")
            wait(lambda: request("receipt", request_id="compact-1")["status"] in ("completed", "failed"), "compaction callback")
            receipt = request("receipt", request_id="compact-1")
            assert receipt["status"] == "completed", receipt
            print(f"Native Pi startup, exact identity, structured send, duplicate suppression and compaction passed ({Model.calls} loopback model calls).")
        finally:
            if tmux:
                subprocess.run([*tmux, "kill-server"], check=True)
            else:
                os.killpg(child.pid, signal.SIGTERM)
                try:
                    child.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    os.killpg(child.pid, signal.SIGKILL)
                    child.wait()
                os.close(master)
            model.shutdown()


if __name__ == "__main__":
    main()
