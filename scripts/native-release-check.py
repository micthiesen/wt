#!/usr/bin/env python3
"""Install, update and recover real native binaries through a loopback release API."""

from __future__ import annotations

import argparse
import http.server
import importlib.util
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading


ROOT = Path(__file__).resolve().parent.parent


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, default=ROOT / "target/debug/wt")
    parser.add_argument("--launcher", type=Path, default=ROOT / "target/debug/wt-launcher")
    args = parser.parse_args()
    binary = args.binary.resolve()
    probe = subprocess.check_output([str(binary), "--_boot-probe"], text=True).strip()
    prefix, build, target = probe.split(":")
    if prefix != "wt-build-id" or len(build) not in {40, 64}:
        raise SystemExit("build wt-app with WT_BUILD_ID set to the complete Git SHA first")
    spec = importlib.util.spec_from_file_location("wt_packaging", ROOT / "scripts/package-native.py")
    packaging = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(packaging)

    with tempfile.TemporaryDirectory(prefix="wt-native-release-") as temporary:
        scratch = Path(temporary)
        home = scratch / "home"
        (home / ".config/wt").mkdir(parents=True)
        (home / ".config/wt/config.toml").write_text("")
        bin_dir = scratch / "build"
        bin_dir.mkdir()
        shutil.copy2(binary, bin_dir / "wt")
        shutil.copy2(args.launcher, bin_dir / "wt-launcher")
        responses: dict[str, bytes] = {}

        class Handler(http.server.BaseHTTPRequestHandler):
            def do_GET(self) -> None:
                body = responses.get(self.path)
                self.send_response(200 if body is not None else 404)
                self.send_header("Content-Length", str(len(body or b"")))
                self.end_headers()
                if body:
                    self.wfile.write(body)

            def log_message(self, *_: object) -> None:
                pass

        server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            api = f"http://127.0.0.1:{server.server_port}"
            for tag in ["v0.0.1", "v0.0.2", "v0.0.3"]:
                output = scratch / tag
                packaging.package(argparse.Namespace(release=tag, build_id=build, target=target,
                                                     bin_dir=bin_dir, output=output))
                archive = output / f"wt-{tag}-{target}.tar.gz"
                artifact = {"target": target, "filename": archive.name,
                            "size": archive.stat().st_size, "sha256": packaging.digest(archive)}
                manifest = json.dumps({"schema_version": 1, "release_version": tag,
                                       "build_id": build, "artifacts": [artifact]}).encode()
                archive_path = f"/assets/{tag}/{archive.name}"
                manifest_path = f"/assets/{tag}/wt-release.json"
                responses[archive_path] = archive.read_bytes()
                responses[manifest_path] = manifest
                metadata = json.dumps({
                    "tag_name": tag, "draft": False, "prerelease": False,
                    "assets": [
                        {"name": archive.name, "size": artifact["size"],
                         "browser_download_url": api + archive_path},
                        {"name": "wt-release.json", "size": len(manifest),
                         "browser_download_url": api + manifest_path},
                    ],
                }).encode()
                responses[f"/repos/fixture/wt/releases/tags/{tag}"] = metadata
            responses["/repos/fixture/wt/releases/latest"] = responses["/repos/fixture/wt/releases/tags/v0.0.2"]
            corrupt = f"/assets/v0.0.3/wt-v0.0.3-{target}.tar.gz"
            responses[corrupt] = responses[corrupt][:-1] + bytes([responses[corrupt][-1] ^ 1])

            env = {key: value for key, value in os.environ.items() if not key.startswith("WT_")}
            root = scratch / "installation"
            env.update({"HOME": str(home), "XDG_CONFIG_HOME": str(home / ".config"),
                        "XDG_DATA_HOME": str(home / ".local/share"),
                        "XDG_CACHE_HOME": str(home / ".cache"), "WT_INSTALL_ROOT": str(root),
                        "WT_RELEASE_REPOSITORY": "fixture/wt", "WT_RELEASE_API_BASE": api})

            def run(executable: Path, *arguments: str, expected: int = 0) -> str:
                result = subprocess.run([str(executable), *arguments], cwd=scratch, env=env,
                                        capture_output=True, text=True, timeout=45)
                assert result.returncode == expected, (arguments, result.returncode, result.stdout, result.stderr)
                return result.stdout + result.stderr

            state_path = root / "state.json"

            def state() -> dict:
                return json.loads(state_path.read_text())

            run(binary, "install", "--release", "v0.0.1", "--path")
            stable = home / ".local/bin/wt"
            assert stable.resolve() == (root / "bin/wt").resolve()
            assert build in run(stable, "version")
            assert state()["pendingBoot"] is not None
            # Missing repo configuration is an ordinary command error. It must
            # confirm executable health without rolling back or replaying argv.
            run(stable, "status", expected=1)
            assert state()["lastGood"]["releaseVersion"] == "v0.0.1"
            before = state()
            before["futurePolicy"] = {"preserve": [1, 2, 3]}
            state_path.write_text(json.dumps(before))
            assert "update available" in run(stable, "update", "--check")
            assert state()["current"]["releaseVersion"] == "v0.0.1"
            run(stable, "update")
            assert state()["current"]["releaseVersion"] == "v0.0.2"
            run(stable, "status", expected=1)
            assert state()["lastGood"]["releaseVersion"] == "v0.0.2"
            run(stable, "rollback")
            assert state()["current"]["releaseVersion"] == "v0.0.1"
            run(stable, "status", expected=1)
            assert "declined" in run(stable, "update", "--check")
            current = state()["current"]
            output = run(stable, "update", "--release", "v0.0.3", expected=1)
            assert "checksum" in output.lower() or "hash" in output.lower(), output
            assert state()["current"] == current
            run(stable, "update", "--release", "v0.0.2")
            pending = state()["current"]
            candidate = root / "versions" / "-".join(
                pending[key] for key in ["releaseVersion", "buildId", "target"]
            ) / "bin/wt"
            candidate.write_text("#!/bin/sh\nexit 2\n")
            candidate.chmod(0o755)
            run(stable, "status", expected=1)
            assert state()["current"]["releaseVersion"] == "v0.0.1"
            assert state()["futurePolicy"] == {"preserve": [1, 2, 3]}
            print(json.dumps({"real_native_install": True, "update_and_rollback": True,
                              "config_error_does_not_rollback": True, "tamper_rejected": True,
                              "pending_probe_recovers": True, "unknown_state_preserved": True}))
        finally:
            server.shutdown()
            server.server_close()
            thread.join(timeout=5)


if __name__ == "__main__":
    main()
