#!/usr/bin/env python3
"""Version checks and curated release notes for native-only Noria builds."""

from pathlib import Path, PurePosixPath
import hashlib
import re
import sys
import tarfile

ROOT = Path(__file__).resolve().parent.parent
VERSION = (ROOT / "VERSION").read_text().strip()
CHANGELOG = ROOT / "docs/src/content/docs/changelog.md"
VERSION_PATTERN = re.compile(r"^\d+\.\d+\.\d+$")


def check(tag: str) -> str:
    if not VERSION_PATTERN.fullmatch(VERSION) or tag != "v" + VERSION:
        raise ValueError(f"Release tag must exactly match VERSION: v{VERSION}")
    manifest = (ROOT / "Cargo.toml").read_text()
    package = re.search(r"(?ms)^\[package\]\s*\n(.*?)(?=^\[|\Z)", manifest)
    if package is None or not re.search(r'(?m)^version\s*=\s*"' + re.escape(VERSION) + r'"\s*$', package[1]):
        raise ValueError("Cargo.toml package.version must match VERSION")
    return VERSION


def expected_artifacts():
    names = set()
    for system in ("darwin", "linux", "windows"):
        for arch in ("amd64", "arm64"):
            base = f"noria_{system}_{arch}"
            names.update((base + (".exe" if system == "windows" else ""), base + ".tar.gz"))
    return names


def validate_archive(path: Path, system: str):
    """Reject incomplete bundles, traversal, duplicate paths, and unsafe links."""
    required = {"LICENSE", "THIRD_PARTY_NOTICES.md", "THIRD_PARTY_LICENSES.txt"}
    executables = {"noria.exe", "noria-tray.exe"} if system == "windows" else {"noria"}
    if system == "linux":
        executables.add("noria-tray")
    if system == "darwin":
        executables.add("Noria.app/Contents/MacOS/noria-menubar")
        required.update(("Noria.app/Contents/Info.plist", "Noria.app/Contents/_CodeSignature/CodeResources"))
    required.update(executables)
    seen = {}
    with tarfile.open(path, "r:gz") as archive:
        for entry in archive:
            name = PurePosixPath(entry.name)
            if name.is_absolute() or ".." in name.parts or "\\" in entry.name or not name.parts:
                raise ValueError(f"Unsafe archive path: {entry.name}")
            if entry.name in seen:
                raise ValueError(f"Duplicate archive entry: {entry.name}")
            if not entry.isfile() and not entry.isdir():
                raise ValueError(f"Unexpected archive link or special file: {entry.name}")
            seen[entry.name] = entry
        for name in required:
            if name not in seen or not seen[name].isfile() or seen[name].size == 0:
                raise ValueError(f"Missing or empty archive member: {name}")
        if system != "windows":
            for name in executables:
                if seen[name].mode & 0o111 == 0:
                    raise ValueError(f"Archive executable lacks execute permission: {name}")


def checksums(directory: Path):
    names = expected_artifacts()
    actual = {path.name for path in directory.glob("noria_*") if path.is_file()}
    if actual != names:
        raise ValueError(f"Release artifact mismatch; missing={sorted(names - actual)}, unexpected={sorted(actual - names)}")
    for name in sorted(names):
        path = directory / name
        if path.stat().st_size == 0:
            raise ValueError(f"Empty release artifact: {name}")
        if name.endswith(".tar.gz"):
            validate_archive(path, name.split("_")[1])
    lines = []
    for name in sorted(names):
        with (directory / name).open("rb") as source:
            lines.append(hashlib.file_digest(source, "sha256").hexdigest() + "  " + name)
    (directory / "SHA256SUMS").write_text("\n".join(lines) + "\n")


def verify(directory: Path):
    expected = expected_artifacts()
    records = {}
    for line in (directory / "SHA256SUMS").read_text().splitlines():
        match = re.fullmatch(r"([a-f0-9]{64})  ([^/\\]+)", line)
        if not match or match[2] in records:
            raise ValueError("Malformed or duplicate SHA256SUMS entry")
        records[match[2]] = match[1]
    if set(records) != expected:
        raise ValueError("SHA256SUMS must contain exactly six binaries and six desktop archives")
    if {path.name for path in directory.glob("noria_*") if path.is_file()} != expected:
        raise ValueError("Release directory does not contain exactly the supported artifacts")
    for name, digest in records.items():
        with (directory / name).open("rb") as source:
            if hashlib.file_digest(source, "sha256").hexdigest() != digest:
                raise ValueError(f"Checksum mismatch: {name}")
        if name.endswith(".tar.gz"):
            validate_archive(directory / name, name.split("_")[1])


def notes(tag: str) -> str:
    version = check(tag)
    source = CHANGELOG.read_text()
    match = re.search(r"(?m)^## v?" + re.escape(version) + r"(?:\s+[^\n]*)?\n", source)
    if not match:
        raise ValueError(f"Changelog is missing manual release notes for {version}")
    body = re.split(r"(?m)^## ", source[match.end():], maxsplit=1)[0].strip()
    if not body:
        raise ValueError(f"Empty release notes for {version}")
    body = body.replace("](/docs/", "](https://noria.ashray.xyz/docs/")
    return (
        body
        + "\n\nInstall macOS/Linux: `curl -fsSL https://noria.ashray.xyz/install.sh | sh`"
        + "\n\nInstall Windows: `irm https://noria.ashray.xyz/install.ps1 | iex`"
        + "\n\nUpgrade: `noria upgrade`"
        + "\n\nDocumentation: https://noria.ashray.xyz\n"
    )


def main() -> int:
    if len(sys.argv) != 3 or sys.argv[1] not in ("check", "notes", "checksums", "verify"):
        print("Usage: python3 scripts/release.py check|notes vX.Y.Z | checksums|verify DIST", file=sys.stderr)
        return 2
    try:
        if sys.argv[1] == "check":
            check(sys.argv[2])
        elif sys.argv[1] == "notes":
            print(notes(sys.argv[2]), end="")
        elif sys.argv[1] == "checksums":
            checksums(Path(sys.argv[2]))
        else:
            verify(Path(sys.argv[2]))
        return 0
    except (ValueError, OSError, tarfile.TarError) as error:
        print(error, file=sys.stderr)
        return 1


if __name__ == "__main__":
    sys.exit(main())
