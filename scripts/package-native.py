#!/usr/bin/env python3
"""Build and assemble wt's immutable native release assets (CI only)."""

import argparse
import gzip
import hashlib
import io
import json
import os
from pathlib import Path
import re
import shutil
import subprocess
import tarfile

TARGETS = {
    "aarch64-apple-darwin",
    "x86_64-apple-darwin",
    "aarch64-unknown-linux-gnu",
    "x86_64-unknown-linux-gnu",
}


def identity(release, build_id, target=None):
    if not re.fullmatch(r"[A-Za-z0-9][A-Za-z0-9._+-]*", release):
        raise ValueError("release must be a safe, nonempty filename component")
    if not re.fullmatch(r"[0-9a-f]{40}|[0-9a-f]{64}", build_id):
        raise ValueError("build ID must be the complete Git commit SHA")
    if target is not None and target not in TARGETS:
        raise ValueError(f"unsupported native release target: {target}")


def digest(path):
    value = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def write_json(path, value):
    path.write_text(json.dumps(value, indent=2, sort_keys=True) + "\n")


def package(args):
    identity(args.release, args.build_id, args.target)
    binary = args.bin_dir.resolve() / "wt"
    launcher = args.bin_dir.resolve() / "wt-launcher"
    if not launcher.is_file():
        raise ValueError("the stable wt-launcher binary was not built")
    probe = subprocess.run([str(binary), "--_boot-probe"], capture_output=True, timeout=20, check=True)
    expected = f"wt-build-id:{args.build_id}:{args.target}\n".encode()
    if probe.stdout != expected:
        raise ValueError(f"binary probe disagrees with requested release identity: {probe.stdout!r}")
    args.output.mkdir(parents=True, exist_ok=True)
    stem = f"wt-{args.release}-{args.target}"
    archive = args.output / f"{stem}.tar.gz"
    build_info = json.dumps({"schema_version": 1, "release_version": args.release,
                             "build_id": args.build_id, "target": args.target}, sort_keys=True).encode()
    epoch = int(os.environ.get("SOURCE_DATE_EPOCH", "0"))
    # Stable member ordering/permissions/ownership/timestamps produce the same
    # archive for the same binaries, independent of runner/user paths.
    with archive.open("wb") as output:
        with gzip.GzipFile(filename="", mode="wb", fileobj=output, mtime=epoch) as compressed:
            with tarfile.open(fileobj=compressed, mode="w", format=tarfile.USTAR_FORMAT) as tar:
                for name, source, mode in [("wt", binary, 0o755), ("wt-launcher", launcher, 0o755),
                                           ("wt-build-info.json", build_info, 0o644)]:
                    data = source.read_bytes() if isinstance(source, Path) else source
                    member = tarfile.TarInfo(f"{stem}/{name}")
                    member.mode, member.mtime, member.size = mode, epoch, len(data)
                    tar.addfile(member, io.BytesIO(data))
    artifact = {"target": args.target, "filename": archive.name, "size": archive.stat().st_size,
                "sha256": digest(archive)}
    write_json(args.output / f"{stem}.artifact.json", {"schema_version": 1, "release_version": args.release,
                                                    "build_id": args.build_id, "artifact": artifact})
    print(archive)


def manifest(args):
    identity(args.release, args.build_id)
    artifacts = []
    seen = set()
    for sidecar in sorted(args.output.glob("*.artifact.json")):
        value = json.loads(sidecar.read_text())
        if (value.get("schema_version"), value.get("release_version"), value.get("build_id")) != (1, args.release, args.build_id):
            raise ValueError(f"mixed release identity in {sidecar.name}")
        artifact = value["artifact"]
        target = artifact["target"]
        identity(args.release, args.build_id, target)
        if target in seen:
            raise ValueError(f"duplicate artifact target {target}")
        seen.add(target)
        expected_name = f"wt-{args.release}-{target}.tar.gz"
        if artifact["filename"] != expected_name:
            raise ValueError(f"unexpected archive filename for {target}")
        archive = args.output / expected_name
        if artifact["size"] != archive.stat().st_size or artifact["sha256"] != digest(archive):
            raise ValueError(f"archive no longer matches sidecar: {expected_name}")
        artifacts.append(artifact)
    if seen != TARGETS:
        raise ValueError(f"release target set incomplete: missing {sorted(TARGETS - seen)}")
    write_json(args.output / "wt-release.json", {"schema_version": 1, "release_version": args.release,
                                               "build_id": args.build_id, "artifacts": artifacts})
    checksums = "".join(f"{a['sha256']}  {a['filename']}\n" for a in artifacts)
    if getattr(args, "installer", None) is not None:
        installer = args.output / "install.sh"
        shutil.copyfile(args.installer, installer)
        checksums += f"{digest(installer)}  install.sh\n"
    (args.output / "SHA256SUMS").write_text(checksums)
    print(args.output / "wt-release.json")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    subcommands = parser.add_subparsers(dest="command", required=True)
    for name in ("package", "manifest"):
        command = subcommands.add_parser(name)
        command.add_argument("--release", required=True)
        command.add_argument("--build-id", required=True)
        command.add_argument("--output", type=Path, required=True)
        if name == "package":
            command.add_argument("--target", required=True, choices=sorted(TARGETS))
            command.add_argument("--bin-dir", type=Path, required=True)
        else:
            command.add_argument("--installer", type=Path)
    args = parser.parse_args()
    (package if args.command == "package" else manifest)(args)


if __name__ == "__main__":
    main()
