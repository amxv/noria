#!/usr/bin/env python3
"""Build one native release target. CI assembles all six supported targets."""
import argparse
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import tarfile
import tempfile

import release
from third_party_licenses import write_notices

ROOT = Path(__file__).resolve().parent.parent
TARGETS = {
    ("linux", "amd64"): "x86_64-unknown-linux-musl",
    ("linux", "arm64"): "aarch64-unknown-linux-musl",
    ("darwin", "amd64"): "x86_64-apple-darwin",
    ("darwin", "arm64"): "aarch64-apple-darwin",
    ("windows", "amd64"): "x86_64-pc-windows-msvc",
    ("windows", "arm64"): "aarch64-pc-windows-msvc",
}


def native_target():
    system = platform.system().lower()
    arch = {"x86_64": "amd64", "amd64": "amd64", "aarch64": "arm64", "arm64": "arm64"}.get(platform.machine().lower())
    if (system, arch) not in TARGETS:
        raise ValueError(f"Unsupported native release host: {system}/{platform.machine()}")
    return system, arch


def run(*args, env=None):
    subprocess.run(args, cwd=ROOT, env=env, check=True)


def verify_static_linux_binary(path: Path):
    """Catch toolchain wrappers that silently turn static PIE into dynamic ELF."""
    headers = subprocess.check_output(["readelf", "--program-headers", "--wide", str(path)], text=True)
    dynamic = subprocess.check_output(["readelf", "--dynamic", "--wide", str(path)], text=True)
    if any(line.split()[:1] == ["INTERP"] for line in headers.splitlines()) or "(NEEDED)" in dynamic:
        raise ValueError(f"Linux release binary has a dynamic loader or shared library dependency: {path}")


def build(output: Path, expected_os=None, expected_arch=None):
    system, arch = native_target()
    if (expected_os and expected_os != system) or (expected_arch and expected_arch != arch):
        raise ValueError(f"This job must run natively on {expected_os}/{expected_arch}; actual host is {system}/{arch}")
    version = release.check("v" + release.VERSION)
    output.mkdir(parents=True, exist_ok=True)
    target = TARGETS[system, arch]
    args = ["cargo", "build", "--locked", "--release", "--target", target, "--bin", "noria"]
    if system != "darwin":
        args += ["--features", "desktop", "--bin", "noria-tray"]
    run(*args)
    metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--locked", "--no-deps", "--format-version", "1"], cwd=ROOT))
    binaries = Path(metadata["target_directory"]) / target / "release"
    suffix = ".exe" if system == "windows" else ""
    binary = binaries / ("noria" + suffix)
    if system == "linux":
        verify_static_linux_binary(binary)
        verify_static_linux_binary(binaries / "noria-tray")
    actual = subprocess.check_output([str(binary), "--version"], text=True).strip()
    if actual != f"noria {version}":
        raise ValueError(f"Embedded CLI version mismatch: {actual!r}")
    shutil.copy2(binary, output / f"noria_{system}_{arch}{suffix}")
    with tempfile.TemporaryDirectory(prefix="noria-release-") as temporary:
        stage = Path(temporary)
        shutil.copy2(binary, stage / ("noria" + suffix))
        for filename in ("LICENSE", "THIRD_PARTY_NOTICES.md"):
            shutil.copy2(ROOT / filename, stage / filename)
        write_notices(stage / "THIRD_PARTY_LICENSES.txt")
        shutil.copy2(stage / "THIRD_PARTY_LICENSES.txt", output / "THIRD_PARTY_LICENSES.txt")
        if system == "darwin":
            swift_arch = "x86_64" if arch == "amd64" else "arm64"
            run("bash", "apps/menubar/build.sh", str(stage / "Noria.app"), swift_arch, env=dict(os.environ, NORIA_VERSION=version))
            run("codesign", "--verify", "--deep", "--strict", str(stage / "Noria.app"))
            app = stage / "Noria.app/Contents/MacOS/noria-menubar"
            run(str(app), "--check")
            for flag, fixture in (("--validate-status-json", "status.json"), ("--validate-compact-status-json", "compact-status.json")):
                run(str(app), flag, str(ROOT / "apps/menubar/Tests" / fixture))
        else:
            tray = binaries / ("noria-tray" + suffix)
            run(str(tray), "--check")
            shutil.copy2(tray, stage / tray.name)
        archive = output / f"noria_{system}_{arch}.tar.gz"
        with tarfile.open(archive, "w:gz") as tar:
            for child in sorted(stage.iterdir()):
                tar.add(child, arcname=child.name)
        release.validate_archive(archive, system)
    print(f"Built native {system}/{arch} binary and desktop archive: {output}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "dist")
    parser.add_argument("--os", choices=("darwin", "linux", "windows"))
    parser.add_argument("--arch", choices=("amd64", "arm64"))
    args = parser.parse_args()
    build(args.output.resolve(), args.os, args.arch)


if __name__ == "__main__":
    main()
