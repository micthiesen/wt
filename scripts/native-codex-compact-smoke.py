#!/usr/bin/env python3
"""Exercise the installed Codex CLI through wt's native terminal fallback.

Requires codex, tmux, Python 3, and Cargo. The fake Responses provider binds
only to loopback; all Codex state and tmux sockets live under a private tempdir.
"""

from __future__ import annotations

import http.server
import json
import os
import pathlib
import shutil
import socketserver
import subprocess
import tempfile
import threading
import time


def require(program: str) -> str:
    path = shutil.which(program)
    if not path:
        raise RuntimeError(f"{program} must be installed and available on PATH")
    return path


def run(argv: list[str], *, env: dict[str, str], timeout: int = 20) -> str:
    result = subprocess.run(argv, env=env, text=True, capture_output=True, timeout=timeout)
    if result.returncode:
        raise RuntimeError(f"{argv[0]} {' '.join(argv[1:3])}: {result.stderr.strip()}")
    return result.stdout


class ProviderHandler(http.server.BaseHTTPRequestHandler):
    requests = 0

    def do_POST(self) -> None:  # noqa: N802
        length = int(self.headers.get("content-length", "0"))
        self.rfile.read(length)
        type(self).requests += 1
        index = type(self).requests
        message = {
            "id": f"msg_{index}",
            "type": "message",
            "role": "assistant",
            "status": "completed",
            "content": [{"type": "output_text", "text": "Harmless local compaction smoke."}],
        }
        events = [
            {"type": "response.created", "response": {"id": f"resp_{index}", "status": "in_progress", "output": []}},
            {"type": "response.output_item.done", "output_index": 0, "item": message},
            {"type": "response.completed", "response": {"id": f"resp_{index}", "status": "completed", "output": [message], "usage": {"input_tokens": 20, "output_tokens": 10, "total_tokens": 30}}},
        ]
        body = "".join(f"event: {event['type']}\ndata: {json.dumps(event)}\n\n" for event in events).encode()
        self.send_response(200)
        self.send_header("content-type", "text/event-stream")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)

    def log_message(self, _format: str, *_args: object) -> None:
        pass


class ThreadingServer(socketserver.ThreadingMixIn, http.server.HTTPServer):
    daemon_threads = True
    allow_reuse_address = True


def rollout_events(sessions: pathlib.Path) -> list[dict]:
    events: list[dict] = []
    if not sessions.exists():
        return events
    for path in sessions.rglob("*.jsonl"):
        try:
            for line in path.read_text().splitlines():
                try:
                    events.append(json.loads(line))
                except json.JSONDecodeError:
                    continue
        except OSError:
            continue
    return events


def wait_for(label: str, predicate, deadline_seconds: int, screen) -> None:
    deadline = time.monotonic() + deadline_seconds
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.1)
    raise RuntimeError(f"Timed out waiting for {label}:\n{screen()}")


def main() -> None:
    require("codex")
    require("tmux")
    cargo = require("cargo")
    scratch = pathlib.Path(tempfile.mkdtemp(prefix="wtcc-", dir="/tmp"))
    home = scratch / "home"
    home.mkdir()
    codex_home = home
    cache = scratch / "cache"
    cache.mkdir()
    cwd = scratch / "project"
    cwd.mkdir()
    socket = scratch / "tmux.sock"
    env = {
        "PATH": os.environ["PATH"],
        "HOME": str(home),
        "CODEX_HOME": str(codex_home),
        "TERM": "xterm-256color",
        "TMPDIR": str(scratch),
    }
    provider = ThreadingServer(("127.0.0.1", 0), ProviderHandler)
    provider_thread = threading.Thread(target=provider.serve_forever, daemon=True)
    provider_thread.start()
    base_url = f"http://127.0.0.1:{provider.server_port}/v1"

    def tmux(*args: str) -> str:
        return run(["tmux", "-S", str(socket), "-f", "/dev/null", *args], env=env)

    def screen() -> str:
        try:
            return tmux("capture-pane", "-p", "-t", "=manager-codex:")
        except Exception:
            return "<session unavailable>"

    def compact_count() -> int:
        return sum(item.get("type") == "compacted" for item in rollout_events(codex_home / "sessions"))

    try:
        version = run(["codex", "--version"], env=env).strip()
        print(f"Installed Codex: {version}")
        run([
            "tmux", "-S", str(socket), "-f", "/dev/null", "new-session", "-d",
            "-s", "manager-codex", "-x", "120", "-y", "40", "codex", "-C", str(cwd),
            "--no-alt-screen", "-c", 'model_provider="probe"', "-c", 'model="probe"',
            "-c", 'model_providers.probe.name="probe"', "-c",
            f'model_providers.probe.base_url="{base_url}"', "-c",
            'model_providers.probe.wire_api="responses"', "-c",
            "model_providers.probe.request_max_retries=0", "-c",
            "model_providers.probe.stream_max_retries=0",
            "This is the dedicated wt manager session. Read $manager to initialize, then wait for a request.\n\n<!-- wt:codex-slot=manager:v1 -->",
        ], env=env)
        wait_for("Codex trust prompt or composer", lambda: "Trust and continue" in screen() or "probe default" in screen(), 30, screen)
        if "Trust and continue" in screen():
            tmux("send-keys", "-t", "manager-codex", "Enter")
        wait_for("initialized composer", lambda: "probe default" in screen(), 30, screen)
        sessions = codex_home / "sessions"
        wait_for("completed seed turn", lambda: any(item.get("payload", {}).get("type") == "task_complete" for item in rollout_events(sessions)), 30, screen)
        events = rollout_events(sessions)
        session_id = next((item.get("payload", {}).get("id") for item in events if item.get("type") == "session_meta"), None)
        if not session_id:
            raise RuntimeError("Codex rollout did not persist its session UUID")
        tmux("set-option", "-t", "manager-codex", "@wt-harness-session-id", session_id)
        user_before = [item for item in rollout_events(sessions) if item.get("type") == "response_item" and item.get("payload", {}).get("type") == "message" and item.get("payload", {}).get("role") == "user"]

        # This Rust example calls CodexMessenger::send_target, which enters the
        # production UUID/readiness-checked bare slash-command fallback.
        native_result = run([
            cargo, "run", "--quiet", "-p", "wt-harness", "--example", "codex-compact-smoke", "--",
            str(home), str(codex_home), str(cache), str(socket), str(cwd),
        ], env=env, timeout=180)
        if "terminal-fallback:" not in native_result:
            raise RuntimeError(f"native sender did not report terminal fallback: {native_result}")
        wait_for("persisted compaction event", lambda: compact_count() >= 1, 30, screen)
        time.sleep(0.5)
        if compact_count() != 1:
            raise RuntimeError(f"expected exactly one native Compact event, saw {compact_count()}")
        user_after = [item for item in rollout_events(sessions) if item.get("type") == "response_item" and item.get("payload", {}).get("type") == "message" and item.get("payload", {}).get("role") == "user"]
        if user_before != user_after:
            raise RuntimeError("native /compact created an unexpected user-message turn")
        if ProviderHandler.requests < 2:
            raise RuntimeError("loopback provider did not serve the seed and compaction requests")
        print("PASS native CodexMessenger submitted bare /compact to the exact live UUID after readiness checks.")
        print("PASS Codex dispatched one Compact operation, persisted compacted state, and added no user turn.")
        print("All Codex provider traffic was served by the isolated loopback fake; no credentials were passed.")
    finally:
        try:
            subprocess.run(["tmux", "-S", str(socket), "kill-server"], env=env, timeout=5, capture_output=True)
        except Exception:
            pass
        provider.shutdown()
        provider.server_close()
        provider_thread.join(timeout=3)
        shutil.rmtree(scratch, ignore_errors=True)


if __name__ == "__main__":
    main()
