#!/usr/bin/env python3
"""Exercise native GitHub events over loopback with isolated config and fake launchd tools."""

from __future__ import annotations

import argparse
import hashlib
import hmac
import json
import os
import plistlib
import shutil
import signal
import socket
import subprocess
import tempfile
import time
import urllib.error
import urllib.request
from pathlib import Path


def checked(command: list[str], *, env: dict[str, str], cwd: Path, ok: bool = True) -> subprocess.CompletedProcess[str]:
    result = subprocess.run(command, cwd=cwd, env=env, text=True, capture_output=True, timeout=20)
    if ok and result.returncode != 0:
        raise RuntimeError(f"command failed ({result.returncode}): {command!r}\nstdout: {result.stdout}\nstderr: {result.stderr}")
    if not ok and result.returncode == 0:
        raise RuntimeError(f"command unexpectedly succeeded: {command!r}\nstdout: {result.stdout}")
    return result


def free_port() -> int:
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        return sock.getsockname()[1]


def request(url: str, body: bytes, event: str, secret: str, *, signature: bool = True) -> int:
    req = urllib.request.Request(url, body, method="POST", headers={"X-GitHub-Event": event})
    if signature:
        digest = hmac.new(secret.encode(), body, hashlib.sha256).hexdigest()
        req.add_header("X-Hub-Signature-256", f"sha256={digest}")
    try:
        with urllib.request.urlopen(req, timeout=3) as response:
            return response.status
    except urllib.error.HTTPError as error:
        return error.code


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--binary", type=Path, required=True, help="built native wt executable")
    args = parser.parse_args()
    binary = args.binary.resolve()
    if not binary.is_file() or not os.access(binary, os.X_OK):
        parser.error(f"native binary is not executable: {binary}")

    with tempfile.TemporaryDirectory(prefix="wt-native-events-") as scratch:
        root = Path(scratch)
        home = root / "home"
        config_home = home / ".config"
        cache = root / "cache"
        repo = root / "repo"
        install_root = root / "install"
        worktrees = root / "worktrees"
        fake_bin = root / "fake-bin"
        for directory in (home, config_home / "wt", cache, repo, worktrees, install_root / "bin", fake_bin):
            directory.mkdir(parents=True, exist_ok=True)

        port = free_port()
        secret = "isolated-native-events-secret"
        config_path = config_home / "wt" / "config.toml"
        config_path.write_text(
            f'''[paths]\nmain_clone = "{repo}"\nworktree_root = "{worktrees}"\ncache_db = "{cache / 'query.sqlite'}"\nstate_db = "{cache / 'state.sqlite'}"\n\n[branch]\nprefix = "test"\nbase = "main"\n\n[github.events]\nhost = "127.0.0.1"\nport = {port}\nsecret = "{secret}"\n''',
            encoding="utf-8",
        )
        subprocess.run(["git", "init", "--quiet", str(repo)], check=True, timeout=10)
        stable = install_root / "bin" / "wt"
        shutil.copy2(binary, stable)
        stable.chmod(0o755)

        plutil = fake_bin / "plutil"
        plutil.write_text(
            "#!/usr/bin/env python3\n"
            "import json, plistlib, sys\n"
            "with open(sys.argv[-1], 'rb') as f: print(json.dumps(plistlib.load(f)))\n",
            encoding="utf-8",
        )
        launchctl = fake_bin / "launchctl"
        launchctl.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, pathlib, plistlib, sys\n"
            "log = pathlib.Path(os.environ['WT_TEST_LAUNCH_LOG'])\n"
            "action = sys.argv[1] if len(sys.argv) > 1 else ''\n"
            "state_file = pathlib.Path(os.environ['WT_TEST_LAUNCH_STATE'])\n"
            "loaded = state_file.exists()\n"
            "if action != 'list':\n"
            " with log.open('a') as f: f.write(' '.join(sys.argv[1:]) + '\\n')\n"
            "if action == 'list':\n"
            " print('PID Status Label')\n"
            " if os.environ.get('WT_TEST_LIST_MODE') == 'malformed': print('bad row format')\n"
            " elif loaded: print('- 0 com.wt.events')\n"
            " sys.exit(0)\n"
            "if action == 'unload':\n"
            " mode = os.environ.get('WT_TEST_UNLOAD_MODE', 'success')\n"
            " if mode == 'fail-loaded' and loaded:\n"
            "  print('simulated unload failure', file=sys.stderr); sys.exit(1)\n"
            " if mode == 'fail-absent' and not loaded:\n"
            "  print('service was not loaded', file=sys.stderr); sys.exit(1)\n"
            " state_file.unlink(missing_ok=True)\n"
            " sys.exit(0)\n"
            "if len(sys.argv) > 1 and sys.argv[1] == 'load':\n"
            " state_file.write_text('loaded')\n"
            " p = plistlib.load(open(sys.argv[-1], 'rb'))\n"
            " env = p['EnvironmentVariables']; cfg = pathlib.Path(env['WT_CONFIG']).read_text()\n"
            " # The test config's cache_db line determines the adjacent events directory.\n"
            " line = next(x for x in cfg.splitlines() if x.startswith('cache_db = '))\n"
            " cache_db = pathlib.Path(line.split('=', 1)[1].strip().strip('\\\"'))\n"
            " events = cache_db.parent / 'events'; events.mkdir(parents=True, exist_ok=True)\n"
            " state = {'pid': int(os.environ['WT_TEST_PID']), 'port': 8765, 'writerSha': os.environ['WT_TEST_BUILD'], 'startedAt': 1, 'lastEventAt': None, 'lastFetchAt': None, 'eventCount': 0, 'lastError': None}\n"
            " (events / 'state.json').write_text(json.dumps(state))\n"
            "sys.exit(0)\n",
            encoding="utf-8",
        )
        plutil.chmod(0o755)
        launchctl.chmod(0o755)

        probe = checked([str(binary), "--_boot-probe"], env=os.environ.copy(), cwd=repo)
        build = probe.stdout.strip().split(":")[1]
        env = os.environ.copy()
        env.update(
            {
                "HOME": str(home),
                "XDG_CONFIG_HOME": str(config_home),
                "WT_CONFIG": str(config_path),
                "WT_INSTALL_ROOT": str(install_root),
                "PATH": f"{fake_bin}:{os.environ.get('PATH', '')}",
                "WT_TEST_LAUNCH_LOG": str(root / "launchctl.log"),
                "WT_TEST_LAUNCH_STATE": str(root / "launchd-loaded"),
                "WT_TEST_PID": str(os.getpid()),
                "WT_TEST_BUILD": build,
            }
        )

        checked([str(binary), "events", "install"], env=env, cwd=repo)
        plist_path = home / "Library" / "LaunchAgents" / "com.wt.events.plist"
        plist = plistlib.loads(plist_path.read_bytes())
        assert plist["ProgramArguments"] == [str(stable), "events", "serve"]
        assert plist["EnvironmentVariables"]["WT_CONFIG"] == str(config_path.resolve()), plist["EnvironmentVariables"]
        owned_env = plist["EnvironmentVariables"]

        # Loading establishes a fake launchd-owned job for unload failure cases.
        checked([str(binary), "events", "start"], env=env, cwd=repo)

        foreign = {
            "Label": "com.wt.events",
            "ProgramArguments": ["/other/repo/bin/wt", "events", "serve"],
            "EnvironmentVariables": {"WT_CONFIG": str(root / "other.toml"), "WT_REPO_CONFIG": ""},
            "StandardOutPath": str(root / "foreign.out"),
            "StandardErrorPath": str(root / "foreign.err"),
        }
        plist_path.write_bytes(plistlib.dumps(foreign))
        before = plist_path.read_bytes()
        result = checked([str(binary), "events", "install"], env=env, cwd=repo, ok=False)
        assert "ownership" in result.stderr or "belongs" in result.stderr
        assert plist_path.read_bytes() == before, "foreign launch agent must remain untouched"

        # A pre-native owned plist can be rewritten and restarted by its stable path.
        events_dir = cache / "events"
        events_dir.mkdir(parents=True, exist_ok=True)
        legacy = {
            "Label": "com.wt.events",
            "ProgramArguments": ["/removed/versioned/bun", "events", "serve"],
            "EnvironmentVariables": owned_env,
            "StandardOutPath": str(events_dir / "daemon.out.log"),
            "StandardErrorPath": str(events_dir / "daemon.err.log"),
        }
        plist_path.write_bytes(plistlib.dumps(legacy))
        legacy_bytes = plist_path.read_bytes()
        (events_dir / "state.json").write_text(json.dumps({"pid": 2147483647, "port": port, "writerSha": "legacy-build", "startedAt": 1, "lastEventAt": None, "lastFetchAt": None, "eventCount": 0, "lastError": None}))
        env["WT_TEST_UNLOAD_MODE"] = "fail-loaded"
        failed_restart = checked([str(binary), "events", "restart"], env=env, cwd=repo, ok=False)
        assert "plist was left in place" in failed_restart.stderr, failed_restart.stderr
        assert plist_path.read_bytes() == legacy_bytes, "failed restart must preserve the active plist"
        env["WT_TEST_LIST_MODE"] = "malformed"
        malformed_restart = checked([str(binary), "events", "restart"], env=env, cwd=repo, ok=False)
        assert "job state is unknown" in malformed_restart.stderr, malformed_restart.stderr
        assert plist_path.read_bytes() == legacy_bytes, "unknown launchd output must preserve the plist"
        del env["WT_TEST_LIST_MODE"]
        env["WT_TEST_UNLOAD_MODE"] = "success"
        checked([str(binary), "events", "restart"], env=env, cwd=repo)
        refreshed = plistlib.loads(plist_path.read_bytes())
        assert refreshed["ProgramArguments"][0] == str(stable)
        state = json.loads((events_dir / "state.json").read_text())
        assert state["pid"] == os.getpid() and state["writerSha"] == build
        assert (root / "launchctl.log").read_text().splitlines()[-2:] == ["unload -w " + str(plist_path), "load -w " + str(plist_path)]

        daemon = subprocess.Popen([str(binary), "events", "serve"], cwd=repo, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        try:
            state_path = events_dir / "state.json"
            deadline = time.monotonic() + 10
            while time.monotonic() < deadline:
                try:
                    state = json.loads(state_path.read_text())
                    if state.get("pid") == daemon.pid and (events_dir / "github.json").exists():
                        break
                except (OSError, json.JSONDecodeError):
                    pass
                time.sleep(0.05)
            else:
                raise RuntimeError("native events daemon did not write state and initial snapshot")

            base = f"http://127.0.0.1:{port}"
            with urllib.request.urlopen(base + "/health", timeout=3) as response:
                assert response.status == 200
            ping = b'{"zen":"fixture"}'
            assert request(base + "/webhook", ping, "ping", secret, signature=False) == 401
            assert request(base + "/webhook", ping, "ping", secret) == 200
            snap = json.loads((events_dir / "github.json").read_text())
            assert isinstance(snap["updatedAt"], int)
            assert isinstance(snap["branches"], list)
            assert isinstance(snap["prs"], dict) and isinstance(snap["mergeQueue"], dict)
            assert snap["writerSha"] == build
            assert (events_dir / "github.touch").is_file()
        finally:
            daemon.send_signal(signal.SIGTERM)
            try:
                daemon.communicate(timeout=10)
            except subprocess.TimeoutExpired:
                daemon.kill()
                daemon.communicate(timeout=3)
                raise RuntimeError("events daemon did not stop after SIGTERM")

        env["WT_TEST_UNLOAD_MODE"] = "fail-loaded"
        failed_uninstall = checked([str(binary), "events", "uninstall"], env=env, cwd=repo, ok=False)
        assert "plist was left in place" in failed_uninstall.stderr, failed_uninstall.stderr
        assert plist_path.exists(), "failed uninstall must preserve the plist"

        # A missing launchd row does not permit removal while the recorded daemon lives.
        state_path = events_dir / "state.json"
        state_path.write_text(json.dumps({"pid": os.getpid(), "port": port, "writerSha": build, "startedAt": 1, "lastEventAt": None, "lastFetchAt": None, "eventCount": 0, "lastError": None}))
        env["WT_TEST_UNLOAD_MODE"] = "success"
        live_state = checked([str(binary), "events", "uninstall"], env=env, cwd=repo, ok=False)
        assert "recorded daemon PID(s) are still alive" in live_state.stderr, live_state.stderr
        assert plist_path.exists(), "uninstall must preserve the plist while its recorded daemon lives"

        # A failed unload is harmless only when launchd confirms the job is absent and the recorded PID is dead.
        state_path.write_text(json.dumps({"pid": 2147483647, "port": port, "writerSha": build, "startedAt": 1, "lastEventAt": None, "lastFetchAt": None, "eventCount": 0, "lastError": None}))
        (root / "launchd-loaded").unlink(missing_ok=True)
        env["WT_TEST_UNLOAD_MODE"] = "fail-absent"
        checked([str(binary), "events", "uninstall"], env=env, cwd=repo)
        assert not plist_path.exists()
        print("native events loopback, HMAC, snapshot, stable-launcher and launchd ownership fixtures passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
