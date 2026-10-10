#!/usr/bin/env python3
"""Exercise scripts/install.sh with isolated fake release assets and HOME."""

from __future__ import annotations

import hashlib
import io
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
INSTALLER = ROOT / "scripts/install.sh"
BUILD_ID = "0123456789abcdef0123456789abcdef01234567"
TAG = "rust-test-0123456789ab-123"
REPOSITORY = "example/wt"
OLD_BUILD_ID = "1111111111111111111111111111111111111111"


def target_for(os_name: str, machine: str) -> str:
    machine = machine.lower()
    if os_name == "Darwin":
        if machine in {"arm64", "aarch64"}:
            return "aarch64-apple-darwin"
        if machine in {"x86_64", "amd64"}:
            return "x86_64-apple-darwin"
    if os_name == "Linux":
        if machine in {"arm64", "aarch64"}:
            return "aarch64-unknown-linux-gnu"
        if machine in {"x86_64", "amd64"}:
            return "x86_64-unknown-linux-gnu"
    raise AssertionError(f"unsupported test platform {os_name} {machine}")


def archive_for(directory: Path, tag: str, target: str, extra: bool = False) -> tuple[str, bytes]:
    stem = f"wt-{tag}-{target}"
    app = f"""#!/bin/sh
if [ "${{1:-}}" = "--_boot-probe" ]; then
  printf '%s\\n' 'wt-build-id:{BUILD_ID}:{target}'
  exit 0
fi
printf '%s\\n' "$@" > "$FAKE_INSTALL_ARGS"
printf '%s\\n' "$WT_INSTALL_ROOT" > "$FAKE_INSTALL_ROOT"
printf '%s\\n' "$WT_RELEASE_REPOSITORY" > "$FAKE_INSTALL_REPO"
""".encode()
    entries = {
        f"{stem}/wt": app,
        f"{stem}/wt-launcher": b"fake launcher payload",
        f"{stem}/wt-build-info.json": (
            '{"schema_version":1,"release_version":"'
            + tag
            + '","build_id":"'
            + BUILD_ID
            + '","target":"'
            + target
            + '"}'
        ).encode(),
    }
    if extra:
        entries[f"{stem}/unexpected"] = b"extra"
    archive_name = f"{stem}.tar.gz"
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode="w:gz") as tar:
        for name, data in entries.items():
            info = tarfile.TarInfo(name)
            info.mode = 0o755 if name.endswith("/wt") else 0o644
            info.size = len(data)
            tar.addfile(info, io.BytesIO(data))
    return archive_name, output.getvalue()


class NativeInstallCheck(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        subprocess.run(
            ["cargo", "build", "-p", "wt-launcher"],
            cwd=ROOT,
            check=True,
            timeout=180,
        )
        target_dir = Path(os.environ.get("CARGO_TARGET_DIR", ROOT / "target"))
        cls.launcher_binary = target_dir / "debug" / "wt-launcher"
        if not cls.launcher_binary.is_file():
            raise AssertionError(f"launcher build did not produce {cls.launcher_binary}")

    def setUp(self) -> None:
        self.scratch = tempfile.TemporaryDirectory(prefix="wt-native-install-check-")
        self.base = Path(self.scratch.name)
        self.fake_bin = self.base / "fake-bin"
        self.fake_bin.mkdir()
        self.home = self.base / "home"
        self.home.mkdir()
        self.temp_dir = self.base / "temporary directory"
        self.temp_dir.mkdir()
        self.args_file = self.base / "install-args"
        self.root_file = self.base / "install-root"
        self.repo_file = self.base / "install-repo"
        self.curl_log = self.base / "curl.log"
        self.target = target_for(platform.system(), platform.machine())

    def tearDown(self) -> None:
        self.scratch.cleanup()

    def setup_fetch(self, *, bad_digest: bool = False, extra: bool = False, duplicate: bool = False) -> None:
        archive_name, archive = archive_for(self.base, TAG, self.target, extra=extra)
        archive_path = self.base / "release.tar.gz"
        archive_path.write_bytes(archive)
        digest = hashlib.sha256(archive).hexdigest()
        if bad_digest:
            digest = "0" * 64
        sums = self.base / "SHA256SUMS"
        sums.write_text(f"{digest}  {archive_name}\n")
        if duplicate:
            sums.write_text(sums.read_text() + f"{digest}  {archive_name}\n")
        curl = self.fake_bin / "curl"
        curl.write_text(
            "#!/bin/sh\n"
            "out=\nurl=\n"
            "while [ \"$#\" -gt 0 ]; do\n"
            "  case $1 in --output|-o) out=$2; shift 2 ;;\n"
            "    --user-agent) printf '%s\\n' \"$2\" >> \"$FAKE_CURL_LOG\"; shift 2 ;;\n"
            "    -*) shift ;;\n"
            "    *) url=$1; shift ;;\n"
            "  esac\n"
            "done\n"
            "case $url in */SHA256SUMS) cp \"$FAKE_SUMS\" \"$out\" ;;\n"
            "  *.tar.gz) cp \"$FAKE_ARCHIVE\" \"$out\" ;;\n"
            "  *) echo \"unexpected URL: $url\" >&2; exit 22 ;;\n"
            "esac\n"
        )
        curl.chmod(0o755)
        uname = self.fake_bin / "uname"
        uname.write_text(
            "#!/bin/sh\ncase ${1:-} in -s) printf '%s\\n' \"${FAKE_UNAME_S:-Linux}\" ;; "
            "-m) printf '%s\\n' \"${FAKE_UNAME_M:-x86_64}\" ;; "
            "*) exit 2 ;; esac\n"
        )
        uname.chmod(0o755)
        # Delegate all non-test commands through the caller's original PATH.
        for command in ("awk", "cut", "tr", "tar", "sha256sum", "shasum", "chmod", "rm", "mktemp"):
            located = shutil.which(command)
            if located:
                (self.fake_bin / command).symlink_to(located)
        self.env = os.environ.copy()
        self.env.update(
            {
            "PATH": f"{self.fake_bin}:{os.environ['PATH']}",
                "HOME": str(self.home),
            "FAKE_UNAME_S": platform.system(),
            "FAKE_UNAME_M": platform.machine(),
                "TMPDIR": str(self.temp_dir),
                "FAKE_SUMS": str(sums),
                "FAKE_ARCHIVE": str(archive_path),
                "FAKE_INSTALL_ARGS": str(self.args_file),
                "FAKE_INSTALL_ROOT": str(self.root_file),
                "FAKE_INSTALL_REPO": str(self.repo_file),
                "FAKE_CURL_LOG": str(self.curl_log),
                "WT_RELEASE_REPOSITORY": REPOSITORY,
            }
        )

    def run_install(self, *arguments: str) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            ["sh", str(INSTALLER), *arguments],
            cwd=self.base,
            env=self.env,
            text=True,
            capture_output=True,
            timeout=30,
            check=False,
        )

    def test_explicit_test_tag_is_checksum_verified_then_passed_to_native_install(self) -> None:
        self.setup_fetch()
        install_root = self.base / "isolated install"
        result = self.run_install("--release", TAG, "--root", str(install_root), "--path")
        self.assertEqual(result.returncode, 0, result.stderr)
        args = self.args_file.read_text().splitlines()
        self.assertEqual(args, ["install", "--release", TAG, "--expected-build-id", BUILD_ID, "--path"])
        self.assertEqual(self.root_file.read_text().strip(), str(install_root))
        self.assertEqual(self.repo_file.read_text().strip(), REPOSITORY)
        self.assertTrue(all(line == "OpenAI File Downloader, XaiImageApiFetch/1.0" for line in self.curl_log.read_text().splitlines()))

    def test_preview_bootstraps_from_stable_without_conflating_build_ids(self) -> None:
        self.setup_fetch()
        result = self.run_install("--channel", "preview", "--root", str(self.base / "install"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.args_file.read_text().splitlines(), ["install", "--channel", "preview"])

    def test_stable_install_pins_native_selection_to_verified_bootstrap_tag(self) -> None:
        self.setup_fetch()
        result = self.run_install("--root", str(self.base / "install"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(
            self.args_file.read_text().splitlines(),
            ["install", "--release", TAG, "--channel", "stable", "--expected-build-id", BUILD_ID],
        )
        self.assertTrue(all(line == "OpenAI File Downloader, XaiImageApiFetch/1.0" for line in self.curl_log.read_text().splitlines()))

    def test_bad_checksum_refuses_to_execute_bootstrap(self) -> None:
        self.setup_fetch(bad_digest=True)
        result = self.run_install("--release", TAG, "--root", str(self.base / "install"))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("checksum mismatch", result.stderr)
        self.assertFalse(self.args_file.exists())

    def test_duplicate_target_checksum_entry_fails_closed(self) -> None:
        self.setup_fetch(duplicate=True)
        result = self.run_install("--release", TAG, "--root", str(self.base / "install"))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("no unique checksum entry", result.stderr)
        self.assertFalse(self.args_file.exists())

    def test_unexpected_archive_entry_is_rejected_before_bootstrap_runs(self) -> None:
        self.setup_fetch(extra=True)
        result = self.run_install("--release", TAG, "--root", str(self.base / "install"))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("unexpected archive entry", result.stderr)
        self.assertFalse(self.args_file.exists())

    def test_supported_target_mapping_includes_all_release_targets(self) -> None:
        self.assertEqual(target_for("Linux", "x86_64"), "x86_64-unknown-linux-gnu")
        self.assertEqual(target_for("Linux", "aarch64"), "aarch64-unknown-linux-gnu")
        self.assertEqual(target_for("Darwin", "arm64"), "aarch64-apple-darwin")
        self.assertEqual(target_for("Darwin", "x86_64"), "x86_64-apple-darwin")

    @unittest.skipUnless(os.name == "posix", "stable launcher uses Unix executable layout")
    def test_real_launcher_reads_prior_state_and_probes_pending_fallback(self) -> None:
        root = self.base / "real install"
        launcher = root / "bin" / "wt"
        launcher.parent.mkdir(parents=True)
        shutil.copy2(self.launcher_binary, launcher)
        launcher.chmod(0o755)

        target = target_for(platform.system(), platform.machine())
        fallback = {
            "releaseVersion": "v0.9.0",
            "buildId": OLD_BUILD_ID,
            "target": target,
        }
        candidate = {
            "releaseVersion": "preview-0123456789abcdef0123456789abcdef01234567",
            "buildId": BUILD_ID,
            "target": target,
        }
        fallback_dir = "-".join((fallback["releaseVersion"], fallback["buildId"], target))
        candidate_dir = "-".join((candidate["releaseVersion"], candidate["buildId"], target))
        fallback_app = root / "versions" / fallback_dir / "bin" / "wt"
        candidate_app = root / "versions" / candidate_dir / "bin" / "wt"
        fallback_app.parent.mkdir(parents=True)
        candidate_app.parent.mkdir(parents=True)
        dispatch_log = self.base / "fallback-argv.log"
        fallback_app.write_text(
            "#!/bin/sh\n"
            f"if [ \"${{1:-}}\" = \"--_boot-probe\" ]; then printf '%s\\n' 'wt-build-id:{OLD_BUILD_ID}:{target}'; exit 0; fi\n"
            f"printf '%s\\n' \"$*\" >> '{dispatch_log}'\n"
            "exit 0\n"
        )
        candidate_app.write_text("#!/bin/sh\nexit 19\n")
        fallback_app.chmod(0o755)
        candidate_app.chmod(0o755)
        state = {
            "formatVersion": 1,
            "channel": "preview",
            "current": candidate,
            "lastGood": fallback,
            "pendingBoot": {
                "candidate": candidate,
                "fallback": fallback,
                "attemptToken": "prior-state-attempt",
                "startedUnix": 1,
            },
            "declinedBuildId": None,
            "lastCheckUnix": 7,
            "history": [],
            "futurePolicy": {"mustSurvive": True},
        }
        (root / "state.json").write_text(json.dumps(state))

        result = subprocess.run(
            [str(launcher), "real-user-argument"],
            cwd=self.base,
            text=True,
            capture_output=True,
            timeout=30,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(dispatch_log.read_text(), "real-user-argument\n")
        after = json.loads((root / "state.json").read_text())
        self.assertEqual(after["current"], fallback)
        self.assertIsNone(after["pendingBoot"])
        self.assertEqual(after["declinedBuildId"], BUILD_ID)
        self.assertEqual(after["futurePolicy"], {"mustSurvive": True})


if __name__ == "__main__":
    unittest.main(verbosity=2)
