#!/usr/bin/env python3
"""Run the complete native end-to-end suite shared by local validation and CI."""

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass, field
import os
from pathlib import Path
import platform
import re
import shlex
import subprocess
import sys
import tempfile

ROOT = Path(__file__).resolve().parents[1]


@dataclass(frozen=True)
class Check:
    name: str
    args: tuple[str, ...]
    env: dict[str, str] = field(default_factory=dict)
    # Measures machine resources, so it runs after the others, alone.
    exclusive: bool = False


def plan(binary: Path, output: Path, system: str) -> list[Check]:
    if system not in {"Darwin", "Linux"}:
        raise ValueError(f"native checks support Linux and macOS, got {system}")
    binary = str(binary)

    def check(name: str, script: str, *args: str, env=None, exclusive=False) -> Check:
        return Check(name, (sys.executable, f"scripts/native-{script}-check.py", *args), env or {},
                     exclusive)

    checks = [
        check("bootstrap installation and launcher recovery", "install"),
        check("release installation, update and rollback", "release", "--binary", binary,
              "--launcher", str(Path(binary).with_name("wt-launcher"))),
        check("terminal liveness and filesystem refresh", "ui", "--binary", binary,
              "--sections", "--feeds", "--output", str(output / "wt-native-ui")),
        check("shutdown with Git children in flight", "ui", "--binary", binary,
              "--exit-signal", "TERM", "--output", str(output / "wt-native-signal")),
        check("terminal handoff and return", "session", "--binary", binary,
              "--output", str(output / "wt-native-session")),
        check("detached lifecycle and CLI prompts", "lifecycle",
              env={"WT_NATIVE_BIN": binary, "WT_GITHUB": "off"}),
        check("cleanup safety", "cleanup", env={"WT_NATIVE_BIN": binary}),
        check("owned resource cleanup", "resource-cleanup", binary),
        check("dev-server lifecycle", "dev", "--binary", binary),
        check("durable action workers", "actions", "--binary", binary,
              "--output", str(output / "wt-native-actions")),
        check("controller service RPC and accepted shutdown drain", "controller", "--binary", binary),
        check("worker protocol and SSH forwarding", "remote", "--binary", binary),
        check("host reconnect and runtime updates", "host", "--binary", binary),
        check("state migration and diagnostics", "command", binary),
        check("SST cleanup", "stages", "--binary", binary),
        check("optimistic creation and navigation", "ui", "--binary", binary,
              "--create", "--create-section", "--output", str(output / "wt-native-create")),
        check("overlapping removal and prompt responsiveness", "ui", "--binary", binary,
              "--removal", "--output", str(output / "wt-native-removal")),
    ]
    if system == "Linux":
        checks.append(check("Linux CPU and RAM workload limits", "workload", "--binary", binary,
                            "--output", str(output / "wt-native-workload"), exclusive=True))
    if system == "Darwin":
        checks.append(check("webhooks and service ownership", "events", "--binary", binary))
    return checks


def run_parallel(checks: list[Check], environment: dict[str, str], jobs: int,
                 run=subprocess.run) -> int:
    """Run shared checks `jobs` at a time, then exclusive ones alone. Each
    check's output is captured and printed whole when it finishes."""
    shared = [check for check in checks if not check.exclusive]
    exclusive = [check for check in checks if check.exclusive]
    if jobs <= 1 or len(shared) <= 1:
        return run_checks(checks, environment, run)

    def one(check: Check):
        try:
            return check, run(check.args, cwd=ROOT, env={**environment, **check.env}, check=False,
                              stdout=subprocess.PIPE, stderr=subprocess.STDOUT)
        except OSError as error:
            return check, error

    failed = 0
    with ThreadPoolExecutor(max_workers=jobs) as pool:
        for done, (check, result) in enumerate(pool.map(one, shared), 1):
            print(f"\n[{done}/{len(checks)}] {check.name}", flush=True)
            print(shlex.join(check.args), flush=True)
            if isinstance(result, OSError):
                print(f"FAILED to start {check.name}: {result}", file=sys.stderr, flush=True)
                failed = failed or 127
                continue
            sys.stdout.write((result.stdout or b"").decode(errors="replace"))
            sys.stdout.flush()
            if result.returncode:
                print(f"FAILED: {check.name} (exit {result.returncode})", file=sys.stderr, flush=True)
                failed = failed or (result.returncode if result.returncode > 0 else 128 - result.returncode)
    if failed:
        return failed
    code = run_checks(exclusive, environment, run, quiet_success=True) if exclusive else 0
    if code == 0:
        print(f"\nAll {len(checks)} native checks passed.", flush=True)
    return code


def run_checks(checks: list[Check], environment: dict[str, str], run=subprocess.run,
               quiet_success: bool = False) -> int:
    for index, check in enumerate(checks, 1):
        print(f"\n[{index}/{len(checks)}] {check.name}", flush=True)
        print(shlex.join(check.args), flush=True)
        try:
            result = run(check.args, cwd=ROOT, env={**environment, **check.env}, check=False)
        except OSError as error:
            print(f"FAILED to start {check.name}: {error}", file=sys.stderr, flush=True)
            return 127
        if result.returncode:
            print(f"FAILED: {check.name} (exit {result.returncode})", file=sys.stderr, flush=True)
            return result.returncode if result.returncode > 0 else 128 - result.returncode
    if not quiet_success:
        print(f"\nAll {len(checks)} native checks passed.", flush=True)
    return 0


def verify_identity(binary: Path, expected: str, run=subprocess.run) -> bool:
    command = (str(binary), "--_boot-probe")
    print(f"Checking candidate identity: {shlex.join(command)}", flush=True)
    try:
        probe = run(command, cwd=ROOT, text=True, capture_output=True, timeout=10, check=False)
    except (OSError, subprocess.TimeoutExpired) as error:
        print(f"Candidate boot probe failed: {error}", file=sys.stderr, flush=True)
        return False
    # This small machine-readable probe is captured for validation, then shown
    # in full. The native checks themselves inherit the live terminal streams.
    print(probe.stdout, end="", flush=True)
    print(probe.stderr, end="", file=sys.stderr, flush=True)
    fields = probe.stdout.strip().split(":")
    if (probe.returncode != 0 or len(fields) != 3 or fields[0] != "wt-build-id"
            or fields[1] != expected or not fields[2]):
        print(f"Candidate build identity does not match WT_BUILD_ID={expected}; "
              "rebuild with that WT_BUILD_ID before running native checks.", file=sys.stderr, flush=True)
        return False
    return True


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=Path("target/debug/wt"))
    parser.add_argument("--output", type=Path, help="retained UI/session/action diagnostics directory")
    parser.add_argument("--build", action="store_true", help="build workspace bins/examples first; otherwise use the existing build")
    parser.add_argument("--jobs", type=int, default=int(os.environ.get("WT_NATIVE_JOBS", "1")),
                        help="checks to run at once (default 1, or WT_NATIVE_JOBS)")
    args = parser.parse_args()
    environment = dict(os.environ)
    if not re.fullmatch(r"[0-9a-fA-F]{40}|[0-9a-fA-F]{64}", environment.get("WT_BUILD_ID", "")):
        parser.error("WT_BUILD_ID must be the full Git SHA used to build the candidate")
    if args.build:
        code = run_checks([Check("build native prerequisites", ("cargo", "build", "--workspace", "--bins", "--examples", "--locked"))], environment)
        if code:
            return code
    binary = args.binary.resolve()
    for required in (binary, binary.with_name("wt-launcher")):
        if not required.is_file() or not os.access(required, os.X_OK):
            parser.error(f"missing executable {required}; run cargo build --workspace --bins --examples --locked with WT_BUILD_ID set, or use --build")
    if not verify_identity(binary, environment["WT_BUILD_ID"]):
        return 1
    output = args.output.resolve() if args.output else Path(tempfile.mkdtemp(prefix="wt-native-check-"))
    output.mkdir(parents=True, exist_ok=True)
    print(f"Native check diagnostics: {output}", flush=True)
    try:
        checks = plan(binary, output, platform.system())
    except ValueError as error:
        parser.error(str(error))
    return run_parallel(checks, environment, args.jobs)


if __name__ == "__main__":
    raise SystemExit(main())
