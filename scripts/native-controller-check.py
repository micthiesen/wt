#!/usr/bin/env python3
"""Exercise controller RPC composition using an isolated allowlisted service."""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[1]


def invoke(binary, args, cwd, env, *, input=None, expected=0):
    result = subprocess.run([str(binary), *args], cwd=cwd, env=env, input=input,
                            text=True, capture_output=True, timeout=15)
    if result.returncode != expected:
        raise AssertionError(f"{args}: exit {result.returncode}, expected {expected}\n{result.stdout}\n{result.stderr}")
    return result


def wait_for(predicate, description, timeout=10):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.02)
    raise AssertionError(f"Timed out waiting for {description}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/wt")
    args = parser.parse_args()
    binary = args.binary.resolve()
    probe = subprocess.run([str(binary), "--_boot-probe"], text=True, capture_output=True, timeout=10, check=True)
    fields = probe.stdout.strip().split(":")
    assert len(fields) == 3 and fields[0] == "wt-build-id", probe.stdout
    if os.environ.get("WT_BUILD_ID"):
        assert fields[1] == os.environ["WT_BUILD_ID"], "candidate differs from WT_BUILD_ID"
    # Short explicit scratch root keeps macOS Unix sockets below sockaddr_un limits.
    with tempfile.TemporaryDirectory(prefix="wtc-", dir="/tmp") as temporary:
        root = Path(temporary).resolve()
        home, repo, worker = root / "h", root / "repo", root / "worker"
        for path in (home, repo, worker):
            path.mkdir()
        env = {key: value for key, value in os.environ.items()
               if key in ("PATH", "LANG", "LC_ALL", "LC_CTYPE", "TZ", "TMPDIR", "TMP", "TEMP")}
        fake_bin = root / "bin"
        fake_bin.mkdir()
        ssh = fake_bin / "ssh"
        ssh.write_text(f"#!{sys.executable}\nfrom pathlib import Path\nPath({str(root / 'unexpected-ssh')!r}).touch()\nraise SystemExit(93)\n")
        ssh.chmod(0o700)
        env["PATH"] = str(fake_bin) + os.pathsep + env.get("PATH", "/usr/bin:/bin")
        env.update(HOME=str(home), XDG_CONFIG_HOME=str(home / ".config"),
                   XDG_CACHE_HOME=str(home / ".cache"), XDG_STATE_HOME=str(home / ".local/state"),
                   GIT_CONFIG_GLOBAL="/dev/null", GIT_CONFIG_NOSYSTEM="1", WT_UPDATE="off", WT_GITHUB="off")
        subprocess.run(["git", "init", "-b", "main", str(repo)], env=env, check=True,
                       capture_output=True, timeout=15)
        fixture = root / "service.py"
        fixture.write_text('''import json, pathlib, sys, time
request = json.load(sys.stdin)
root = pathlib.Path(sys.argv[1])
if request.get("write"):
    (root / "accepted").write_text("accepted")
    deadline = time.monotonic() + 8
    while not (root / "release").exists():
        if time.monotonic() > deadline:
            raise RuntimeError("fixture gate was not released")
        time.sleep(.01)
    (root / "completed").write_text(request["write"])
print(json.dumps({"request": request, "cwd": str(pathlib.Path.cwd())}))
''')
        config = repo / ".wt.toml"
        base_config = (f"[paths]\nmain_clone={json.dumps(str(repo))}\n"
                       f"worktree_root={json.dumps(str(root / 'worktrees'))}\n"
                       f"state_db={json.dumps(str(root / 'state.sqlite'))}\n"
                       f"cache_db={json.dumps(str(root / 'cache/cache.sqlite'))}\n"
                       "[branch]\nprefix='fixture'\n[managed.services]\n"
                       f"fixture={json.dumps([sys.executable, str(fixture), str(root)])}\n")
        config.write_text(base_config)
        env.update(WT_CONFIG=str(root / "empty.toml"), WT_REPO_CONFIG=str(config))
        (root / "empty.toml").write_text("")
        daemons, clients = [], []
        log = (root / "daemon.log").open("w+")

        def start():
            process = subprocess.Popen([str(binary), "controller", "serve"], cwd=repo, env=env,
                                       stdin=subprocess.DEVNULL, stdout=log, stderr=log)
            daemons.append(process)
            def ready():
                if process.poll() is not None:
                    log.flush()
                    raise AssertionError(f"daemon exited {process.returncode}: {(root / 'daemon.log').read_text()}")
                check = subprocess.run([str(binary), "controller", "status"], cwd=repo, env=env,
                                       text=True, capture_output=True, timeout=5)
                return check.returncode == 0
            wait_for(ready, "controller readiness")
            return process

        try:
            daemon = start()
            sockets = list((home / ".local/state/wt/controller").glob("*/rpc.sock"))
            assert len(sockets) == 1, sockets
            socket = sockets[0]
            assert socket.stat().st_mode & 0o777 == 0o600
            assert socket.parent.stat().st_mode & 0o777 == 0o700
            remote_env = {**env, "WT_CONTROLLER_SOCKET": str(socket)}
            # Exec deliberately ignores these invalid config selectors.
            remote_env.update(WT_CONFIG=str(root / "missing-config"), WT_REPO_CONFIG=str(root / "missing-repo-config"))
            reply = invoke(binary, ["controller", "exec", "fixture"], worker, remote_env, input='{"read":true}')
            response = json.loads(reply.stdout)
            assert response == {"request": {"read": True}, "cwd": str(repo)}, response
            unknown = invoke(binary, ["controller", "exec", "unknown"], worker, remote_env, input="{}", expected=75)
            assert "not executed" in unknown.stderr
            (worker / "controller-endpoint").write_text(str(socket))
            fallback_env = {**remote_env, "WT_MANAGED_ROOT": str(worker)}
            fallback_env.pop("WT_CONTROLLER_SOCKET")
            invoke(binary, ["controller", "exec", "fixture"], worker, fallback_env, input="{}")
            # A changed loaded configuration must fail health, not silently keep
            # the old allowlist or old tunnel inventory alive.
            config.write_text(base_config + 'other=["/never/run"]\n')
            stale = invoke(binary, ["controller", "status"], repo, env, expected=1)
            assert "stale" in stale.stderr
            config.write_text(base_config)
            invoke(binary, ["controller", "status"], repo, env)
            client = subprocess.Popen([str(binary), "controller", "exec", "fixture"], cwd=worker,
                                      env=remote_env, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                      stderr=subprocess.PIPE, text=True)
            clients.append(client)
            client.stdin.write('{"write":"durable-fixture-result"}')
            client.stdin.close()
            client.stdin = None
            wait_for(lambda: (root / "accepted").exists(), "accepted mutation")
            daemon.send_signal(signal.SIGTERM)
            time.sleep(0.1)
            assert daemon.poll() is None, "daemon abandoned the accepted mutation during shutdown"
            (root / "release").write_text("continue")
            stdout, stderr = client.communicate(timeout=10)
            assert client.returncode == 0, (stdout, stderr)
            assert (root / "completed").read_text() == "durable-fixture-result"
            assert daemon.wait(timeout=10) == 0
            # Same endpoint can be reacquired after graceful lock/socket cleanup.
            restarted = start()
            assert socket.exists()
            invoke(binary, ["controller", "exec", "fixture"], worker, fallback_env, input="{}")
            restarted.send_signal(signal.SIGTERM)
            assert restarted.wait(timeout=10) == 0
            assert not (root / "unexpected-ssh").exists(), "fixture unexpectedly attempted SSH"
            print(json.dumps({"build": fields[1], "controller_rpc": "passed", "accepted_shutdown_drain": "passed",
                              "endpoint_fallback": "passed", "configuration_health": "passed", "restart": "passed"}))
        except Exception:
            log.flush()
            print((root / "daemon.log").read_text(), file=sys.stderr)
            raise
        finally:
            # Only private fixture processes; release first so accepted writes can drain.
            (root / "release").touch()
            for process in clients + daemons:
                if process.poll() is None:
                    process.terminate()
                    try:
                        process.wait(timeout=12)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(timeout=5)
            log.close()


if __name__ == "__main__":
    main()
