#!/usr/bin/env python3
"""Exercise the native action worker with a private HOME and tmux server.

All configuration, jobs, logs, sockets, and child processes belong to the
requested scratch directory. No configured action or user integration runs.
"""

import argparse
import importlib.util
import json
import os
from pathlib import Path
import shutil
import signal
import subprocess
import time


def load_fixture_module():
    spec = importlib.util.spec_from_file_location(
        "wt_perf_baseline", Path(__file__).with_name("perf-baseline.py")
    )
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def write_job(root, run_id, command, config, repo_config, cwd, *, extra=None):
    run_dir = root / "logs" / "actions" / run_id
    run_dir.mkdir(parents=True)
    selectors = {"WT_CONFIG": str(config), "WT_REPO_CONFIG": str(repo_config)}
    meta = {
        "version": 1,
        "slug": "fixture-action",
        "runId": run_id,
        "kind": "shell",
        "actionId": "native-smoke",
        "actionName": "Native action smoke",
        "prompt": "isolated fixture",
        "affects": [],
        "autoFireKeys": [],
        "startedAt": int(time.time() * 1000),
        "status": "running",
        "futureActionField": {"preserve": ["unknown", "metadata"]},
    }
    if extra:
        meta.update(extra)
    request = {
        "actionKey": "fixture-action",
        "slug": "fixture-action",
        "worktreeRef": None,
        "actionId": "native-smoke",
        "actionName": "Native action smoke",
        "prompt": "isolated fixture",
        "kind": "shell",
        "command": command,
        "cwd": str(cwd),
        "affects": [],
        "external": False,
        "autoFireKeys": [],
        "configSelectors": selectors,
    }
    run = {"meta": meta, "runDir": str(run_dir.resolve()), "command": command, "cwd": str(cwd)}
    job = {"version": 1, "request": request, "run": run, "session": "fixture-action-session"}
    (run_dir / "meta.json").write_text(json.dumps(meta, indent=2) + "\n")
    (run_dir / "job.json").write_text(json.dumps(job, indent=2) + "\n")
    return run_dir


def tmux(env, *args, check=True):
    return run_captured(["tmux", *args], env=env, check=check)


def run_captured(command, **kwargs):
    """Keep captured failure evidence visible in CI, including timeout output."""
    try:
        return subprocess.run(command, capture_output=True, text=True, **kwargs)
    except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
        raise AssertionError(
            f"{error}\nstdout:\n{error.stdout!r}\nstderr:\n{error.stderr!r}"
        ) from error


def wait_for(predicate, timeout=8, detail="condition"):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        if predicate():
            return
        time.sleep(0.02)
    raise AssertionError(f"timed out waiting for {detail}")


def process_is_live(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    result = subprocess.run(["ps", "-o", "stat=", "-p", str(pid)], capture_output=True, text=True)
    return result.returncode == 0 and result.stdout.strip() and not result.stdout.strip().startswith("Z")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--service-fixture",
        type=Path,
        default=Path(__file__).resolve().parent.parent / "target/debug/examples/action-service-fixture",
    )
    args = parser.parse_args()
    binary = args.binary.resolve()
    service_fixture = args.service_fixture.resolve()
    root = args.output.resolve()
    if root.exists():
        raise SystemExit(f"Refusing to overwrite existing smoke directory: {root}")
    if not binary.is_file():
        parser.error(f"native wt binary does not exist: {binary}")
    if not service_fixture.is_file():
        parser.error(f"action service fixture binary does not exist: {service_fixture}")
    if shutil.which("tmux") is None:
        parser.error("tmux is required for the native action worker smoke")

    root.mkdir(parents=True)
    main_clone, home, global_config = load_fixture_module().fixture(root / "fixture", 0)
    repo_config = root / "repo-config.toml"
    repo_config.write_text(
        "\n".join(
            [
                "[paths]",
                f"main_clone = {json.dumps(str(main_clone))}",
                f"worktree_root = {json.dumps(str(root / 'fixture/worktrees'))}",
                f"cache_db = {json.dumps(str(root / 'repo-cache.sqlite'))}",
                f"state_db = {json.dumps(str(root / 'repo-state.sqlite'))}",
                "[branch]",
                'prefix = "fixture"',
                'base = "main"',
                "",
            ]
        )
    )
    socket = f"wt-actions-{os.getpid()}"
    env = dict(
        os.environ,
        HOME=str(home),
        XDG_CONFIG_HOME=str(root / "xdg/config"),
        XDG_CACHE_HOME=str(root / "xdg/cache"),
        XDG_STATE_HOME=str(root / "xdg/state"),
        WT_CONFIG=str(global_config),
        WT_REPO_CONFIG=str(repo_config),
        WT_TMUX_SOCKET=socket,
        WT_UPDATE="off",
        WT_SKILLS="off",
        WT_GITHUB="off",
        WT_AUTOMATIONS="off",
        TERM="xterm-256color",
    )
    for key in ("WT_REPO_ID", "WT_AGENT", "BUN_INSPECT"):
        env.pop(key, None)
    tmux(env, "-L", socket, "new-session", "-d", "-s", "smoke-hold", "/bin/sleep", "60")
    tmux(env, "-L", socket, "set-option", "-g", "remain-on-exit", "on")
    # Keep the private server alive through both worker phases. Killing its
    # sole session here destroys the configured server; a replacement loses
    # remain-on-exit and may exit between the next service's read-only probes.
    # The finally block owns this holder and the rest of the server together.

    run_dir = write_job(
        root,
        "live-action",
        [
            "/bin/sh",
            "-c",
            "printf 'SELECTORS:%s|%s|%s\\n' \"$WT_CONFIG\" \"$WT_REPO_CONFIG\" \"$PWD\"; "
            "printf 'live-stdout\\n'; printf 'live-stderr\\n' >&2; sleep 1; "
            "printf 'done-stdout\\n'; printf 'done-stderr\\n' >&2",
        ],
        global_config,
        repo_config,
        main_clone,
    )
    session = "action-live"
    command = [
        "env",
        f"WT_CONFIG={global_config}",
        f"WT_REPO_CONFIG={repo_config}",
        "WT_UPDATE=off",
        "WT_SKILLS=off",
        "WT_GITHUB=off",
        "WT_AUTOMATIONS=off",
        str(binary),
        "_action-worker",
        "--job",
        str(run_dir / "job.json"),
    ]
    tmux(env, "-L", socket, "new-session", "-d", "-s", session, "-c", str(main_clone), *command)
    try:
        stdout_log = run_dir / "stream.log"
        stderr_log = run_dir / "stderr.log"

        def live_streams_visible():
            return (
                stdout_log.exists()
                and stderr_log.exists()
                and b"live-stdout" in stdout_log.read_bytes()
                and b"live-stderr" in stderr_log.read_bytes()
            )

        try:
            wait_for(live_streams_visible, detail="live stdout and stderr log writes")
        except AssertionError as error:
            pane = tmux(env, "-L", socket, "capture-pane", "-p", "-t", session, check=False)
            app_logs = list((root / "logs/app").glob("*.log"))
            app_log = app_logs[0].read_text() if app_logs else "(no app log created)"
            raise AssertionError(
                f"{error}; pane={pane.stdout!r}; app log="
                f"{app_log!r}"
            ) from error
        assert not (run_dir / "done.json").exists(), "worker completed before its live log was observed"
        wait_for(lambda: (run_dir / "done.json").exists(), detail="durable completion metadata")
        done = json.loads((run_dir / "done.json").read_text())
        meta = json.loads((run_dir / "meta.json").read_text())
        output = stdout_log.read_text()
        errors = stderr_log.read_text()
        assert done["status"] == "succeeded", done
        assert str(global_config) in output and str(repo_config) in output and str(main_clone) in output
        assert "done-stdout" in output and "done-stderr" in errors
        assert meta["futureActionField"] == {"preserve": ["unknown", "metadata"]}
        assert tmux(env, "-L", socket, "show-option", "-gv", "remain-on-exit").stdout.strip() == "on", \
            "the configured private tmux server did not survive the first worker"

        def check_service(service_root, service_socket, service_repo_config):
            try:
                helper = run_captured(
                    [
                        str(service_fixture), str(binary), str(service_root), str(main_clone),
                        service_socket, str(global_config), str(service_repo_config),
                    ],
                    env=dict(env, WT_TMUX_SOCKET=service_socket, WT_REPO_CONFIG=str(service_repo_config)),
                    check=True,
                    timeout=90,
                )
            except AssertionError as error:
                panes = tmux(env, "-L", service_socket, "list-panes", "-a", "-F",
                             "#{pane_id} #{session_name} dead=#{pane_dead} status=#{pane_dead_status}", check=False)
                captures = []
                for line in panes.stdout.splitlines()[:8]:
                    capture = tmux(env, "-L", service_socket, "capture-pane", "-p", "-t", line.split()[0], check=False)
                    captures.append(f"{line}\n{capture.stdout}\n{capture.stderr}")
                raise AssertionError(f"{error}\nworker panes: {panes.stdout!r} {panes.stderr!r}\n" + "\n".join(captures)) from error
            service_lines = helper.stdout.strip().splitlines()
            assert len(service_lines) >= 3, helper.stdout
            service_run_id, service_run_dir, service_session = service_lines[-3:]
            service_run_dir = Path(service_run_dir)
            service_pid = int((service_root / "child.pid").read_text())
            wait_for(lambda: (service_run_dir / "done.json").exists(), detail="service kill completion")
            service_done = json.loads((service_run_dir / "done.json").read_text())
            assert service_done["status"] == "killed", service_done
            wait_for(lambda: not process_is_live(service_pid), detail="service descendant reaping")
            assert service_run_id in service_run_dir.name

        check_service(root, socket, repo_config)
        # Production headless actions must also start with no tmux server.
        # Exercise that contract separately from the configured warm server.
        cold_root = root / "cold-service"
        cold_root.mkdir()
        cold_config = cold_root / "repo-config.toml"
        cold_config.write_text(repo_config.read_text().replace(
            "[paths]\n", "[paths]\n" +
            f"log_dir = {json.dumps(str(cold_root / 'logs'))}\n" +
            f"lock_dir = {json.dumps(str(cold_root / 'locks'))}\n"
        ))
        cold_socket = socket + "-cold"
        absent = tmux(env, "-L", cold_socket, "has-session", check=False)
        assert absent.returncode != 0 and (
            "no server running" in absent.stderr or "no such file or directory" in absent.stderr.lower()
        ), absent
        check_service(cold_root, cold_socket, cold_config)

        result = {
            "selector_paths_preserved": True,
            "live_stdout_and_stderr": True,
            "durable_done_record": done["status"],
            "unknown_metadata_preserved": True,
            "service_duplicate_guard": "AlreadyRunning",
            "service_kill_status": "killed",
            "service_descendant_reaped": True,
            "service_cold_server_start": True,
            "private_tmux_socket": socket,
        }
        (root / "result.json").write_text(json.dumps(result, indent=2) + "\n")
        print(json.dumps(result))
    finally:
        tmux(env, "-L", socket, "kill-server", check=False)
        tmux(env, "-L", socket + "-cold", "kill-server", check=False)


if __name__ == "__main__":
    main()
