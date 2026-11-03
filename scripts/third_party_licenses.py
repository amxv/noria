#!/usr/bin/env python3
"""Collect complete upstream notices from the exact locked Cargo dependencies."""
import json
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
TARGETS = (
    "x86_64-unknown-linux-musl", "aarch64-unknown-linux-musl",
    "x86_64-apple-darwin", "aarch64-apple-darwin",
    "x86_64-pc-windows-msvc", "aarch64-pc-windows-msvc",
)


def write_notices(destination: Path):
    packages = {}
    # Cargo's packages list includes inactive targets. Walk the resolved graph
    # for each release target so unused macOS tray backends and dev-only crates
    # are not accidentally represented as components of the shipped binaries.
    for target in TARGETS:
        metadata = json.loads(subprocess.check_output([
            "cargo", "metadata", "--locked", "--all-features", "--format-version", "1",
            "--filter-platform", target,
        ], cwd=ROOT))
        by_id = {package["id"]: package for package in metadata["packages"]}
        nodes = {node["id"]: node for node in metadata["resolve"]["nodes"]}
        pending = [metadata["resolve"]["root"]]
        seen = set()
        while pending:
            identifier = pending.pop()
            if identifier in seen:
                continue
            seen.add(identifier)
            packages[identifier] = by_id[identifier]
            for dependency in nodes[identifier]["deps"]:
                if any(kind["kind"] != "dev" for kind in dependency["dep_kinds"]):
                    pending.append(dependency["pkg"])
    sections = ["Noria Rust dependency licenses\n\nGenerated from Cargo.lock. Includes target-specific dependencies.\n"]
    for package in sorted(packages.values(), key=lambda item: (item["name"], item["version"])):
        if not package.get("source"):
            continue
        root = Path(package["manifest_path"]).parent
        paths = set()
        paths.update(path for path in root.iterdir() if path.is_file() and path.name.upper().startswith(("LICENSE", "LICENCE", "COPYING", "NOTICE", "UNLICENSE")))
        if package.get("license_file"):
            paths.add(root / package["license_file"])
        for folder in ("licenses", "LICENSES"):
            if (root / folder).is_dir():
                paths.update(path for path in (root / folder).rglob("*") if path.is_file())
        curated = None
        if not paths:
            catalog = json.loads((ROOT / "scripts/licenses/sources.json").read_text(encoding="utf-8"))
            curated = catalog.get(package['name'] + '@' + package['version'])
            if not curated:
                raise ValueError(f"No packaged or reviewed upstream license text for {package['name']} {package['version']}")
        sections.append(f"\n{'=' * 72}\n{package['name']} {package['version']}\nSPDX: {package.get('license', 'See included license')}\nSource: {package.get('repository') or package['source']}\n")
        if curated:
            sections.append(f"\nUpstream license: {curated['url']}\n{(ROOT / 'scripts/licenses' / curated['file']).read_text(encoding='utf-8')}\n")
            sections.append(f"\nAuthors listed by upstream: {', '.join(package.get('authors', []))}\n")
            sections.append(f"\nLicense text referenced by upstream: {curated['license_text_url']}\n{(ROOT / 'scripts/licenses' / curated['license_text']).read_text(encoding='utf-8')}\n")
        for path in sorted(paths):
            sections.append(f"\n--- {path.relative_to(root)} ---\n{path.read_text(encoding='utf-8', errors='replace')}\n")
    destination.write_text("".join(sections), encoding="utf-8")


if __name__ == "__main__":
    write_notices(Path(sys.argv[1]))
